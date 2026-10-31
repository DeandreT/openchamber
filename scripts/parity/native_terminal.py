"""Qualify the native terminal against an owned real engine.

Runs the parity seed's isolated server behind a loopback proxy that records
terminal commands and can sever the terminal socket. The native window runs on
the owned Xvfb display with a private IBus (libpinyin). Input reaches the
terminal as X key events (xdotool), AT-SPI string synthesis and IBus
composition; what the PTY received is read back from files that small capture
programs inside the terminal write. Server outcomes come from
`GET /api/terminal/sessions` and the proxy's request log.

Covers `docs/rust-desktop` stage-4 contract section 8.1: create and close,
Unicode and IME input, keys with and without application cursor mode, resize,
bracketed and large paste through the shortcut and the context menu,
scrollback, reconnect with snapshot replay, exit, lifetime and cleanup.

Run: python3 scripts/parity/virtual_display.py -- python3 scripts/parity/native_terminal.py --output <new directory>
"""

import argparse
from contextlib import ExitStack
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import threading
import time
import traceback
import urllib.parse
import urllib.request

from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, wait_for
import runner
import virtual_display

UNICODE_SAMPLE = "héllo 日本語 \U0001f469‍\U0001f4bb é مرحبا ─│"

# Legacy encodings of Ghostty's key encoder, the same encoder the reference
# runs as WebAssembly: (xdotool key, normal bytes, application-cursor bytes).
KEYS = [
    ("Up", b"\x1b[A", b"\x1bOA"),
    ("Down", b"\x1b[B", b"\x1bOB"),
    ("Right", b"\x1b[C", b"\x1bOC"),
    ("Left", b"\x1b[D", b"\x1bOD"),
    ("Home", b"\x1b[H", b"\x1bOH"),
    ("End", b"\x1b[F", b"\x1bOF"),
    ("F1", b"\x1bOP", b"\x1bOP"),
    ("F2", b"\x1bOQ", b"\x1bOQ"),
    ("F5", b"\x1b[15~", b"\x1b[15~"),
    ("F12", b"\x1b[24~", b"\x1b[24~"),
    ("Delete", b"\x1b[3~", b"\x1b[3~"),
    ("Prior", b"\x1b[5~", b"\x1b[5~"),
    ("ctrl+a", b"\x01", b"\x01"),
    ("ctrl+e", b"\x05", b"\x05"),
    ("ctrl+k", b"\x0b", b"\x0b"),
    ("alt+b", b"\x1bb", b"\x1bb"),
    ("BackSpace", b"\x7f", b"\x7f"),
    ("Tab", b"\t", b"\t"),
    ("Escape", b"\x1b", b"\x1b"),
]

# Reads one line and writes it to its file, which it creates first.
LINE_PROGRAM = r"""
import sys
with open(sys.argv[1], "wb") as out:
    out.write(sys.stdin.buffer.readline())
"""

# Marks its start in its file, prints a numbered line every 20 ms, then
# marks the end.
LOOP_PROGRAM = r"""
import sys, time
with open(sys.argv[1], "w") as out:
    out.write("started\n")
    out.flush()
    for index in range(1, 301):
        print(f"line-{index}", flush=True)
        time.sleep(0.02)
    out.write("done\n")
"""

# Typed input reaches the app at a few keys a second on a loaded machine
# with software rendering, so waits grow with what was typed.
SECONDS_PER_KEY = 0.4

# A raw-mode capture program the checks start inside the terminal. It can
# enable application cursor keys or bracketed paste first, appends everything
# it reads to its file as it arrives, and stops after `stop` bytes or a 'q'.
CAPTURE_PROGRAM = r"""
import os, sys, termios, tty
path, setup, stop = sys.argv[1], sys.argv[2], int(sys.argv[3])
old = termios.tcgetattr(0)
tty.setraw(0)
try:
    if setup:
        os.write(1, bytes.fromhex(setup))
    data = b""
    with open(path, "wb") as out:
        while True:
            chunk = os.read(0, 65536)
            data += chunk
            out.write(chunk)
            out.flush()
            if (stop and len(data) >= stop) or (not stop and data.endswith(b"q")):
                break
finally:
    termios.tcsetattr(0, termios.TCSADRAIN, old)
"""


def expected_keys(application_cursor):
    """The bytes the KEYS presses produce, then the stopping 'q'."""
    return b"".join(entry[2] if application_cursor else entry[1] for entry in KEYS) + b"q"


def backoff_within_contract(attempts, base=0.5, cap=8.0, slack=0.35):
    """Whether socket attempts while the server refuses keep the reconnect
    pacing: each wait at least the previous one's doubling floor, none past
    the cap, none shorter than the base."""
    gaps = [later - earlier for earlier, later in zip(attempts, attempts[1:])]
    if not gaps:
        return False
    for index, gap in enumerate(gaps):
        floor = min(base * 2 ** index, cap)
        if gap < floor - slack or gap > cap + 1.5:
            return False
    return True


def listed(sessions, status=None):
    """Session IDs of an authoritative terminal list, optionally by status."""
    return sorted(entry["sessionId"] for entry in sessions if status is None or entry.get("status") == status)


class TerminalProxy:
    """A loopback proxy in front of the isolated server. It records terminal
    commands, counts terminal socket upgrades and can sever every open
    terminal socket and refuse new ones until released."""

    def __init__(self, upstream):
        target = urllib.parse.urlsplit(upstream)
        if target.scheme != "http" or target.hostname != "127.0.0.1" or not target.port:
            raise ValueError("the terminal proxy requires a loopback HTTP origin")
        self.host, self.port = target.hostname, target.port
        self.lock = threading.Lock()
        self.commands = []
        self.upgrades = []
        self.refusing = threading.Event()
        self.stopped = threading.Event()
        self.tunnels = set()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.handle(self)

            do_POST = do_PUT = do_DELETE = do_PATCH = do_GET

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def terminal_commands(self, method=None, suffix=None):
        with self.lock:
            return [entry for entry in self.commands
                    if (method is None or entry["method"] == method) and (suffix is None or entry["path"].endswith(suffix))]

    def handle(self, handler):
        path = urllib.parse.urlsplit(handler.path).path
        if handler.headers.get("Upgrade", "").lower() == "websocket":
            if path == "/api/terminal/ws":
                with self.lock:
                    self.upgrades.append(time.monotonic())
                if self.refusing.is_set():
                    handler.send_response(503)
                    handler.send_header("Content-Length", "0")
                    handler.end_headers()
                    return
            self.tunnel(handler, path == "/api/terminal/ws")
            return
        length = int(handler.headers.get("Content-Length", 0))
        body = handler.rfile.read(length) if length else b""
        if path.startswith("/api/terminal/"):
            with self.lock:
                self.commands.append({"method": handler.command, "path": path, "time": time.monotonic(),
                                      "body": json.loads(body) if body else None})
        headers = {key: value for key, value in handler.headers.items()
                   if key.lower() not in ("host", "connection", "transfer-encoding", "content-length")}
        connection = http.client.HTTPConnection(self.host, self.port, timeout=30)
        try:
            connection.request(handler.command, handler.path, body=body, headers=headers)
            response = connection.getresponse()
            data = response.read()
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

    def tunnel(self, handler, terminal):
        upstream = socket.create_connection((self.host, self.port), timeout=10)
        pair = (upstream, handler.connection)
        if terminal:
            with self.lock:
                self.tunnels.add(pair)
        try:
            headers = "\r\n".join(f"{key}: {value}" for key, value in handler.headers.items() if key.lower() != "host")
            upstream.sendall(f"GET {handler.path} HTTP/1.1\r\nHost: {self.host}:{self.port}\r\n{headers}\r\n\r\n".encode())
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
                self.tunnels.discard(pair)
            upstream.close()
            handler.close_connection = True

    def sever(self):
        """Cuts every open terminal socket and refuses new ones."""
        self.refusing.set()
        with self.lock:
            pairs = list(self.tunnels)
        for pair in pairs:
            for connection in pair:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        return len(pairs)

    def release(self):
        self.refusing.clear()

    def open_terminal_sockets(self):
        with self.lock:
            return len(self.tunnels)

    def stop(self):
        self.stopped.set()
        self.sever()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    clients = []
    previous = accessibility_enabled()

    def check(name, run):
        started = time.monotonic()
        try:
            detail = run()
            results.append({"check": name, "passed": True, "seconds": round(time.monotonic() - started, 1),
                            **({"detail": detail} if detail else {})})
            print(f"{name}: passed", flush=True)
        except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration,
                subprocess.SubprocessError, http.client.HTTPException) as error:
            traceback.print_exc()
            results.append({"check": name, "passed": False, "error": str(error)})
            print(f"{name}: failed: {error}", flush=True)
            try:
                clients[-1].screenshot(args.output / f"{name}-failed.png")
            except Exception:  # noqa: BLE001 - evidence is best effort after a failure
                pass

    try:
        with ExitStack() as cleanup:
            state = runner.copy_seed(args.root, args.output, "openchamber-dark", {"terminalCloseOnExit": False})
            # A fixed prompt and no line-editor features the checks would race.
            (state / "home" / ".zshrc").write_text("PROMPT='$ '\nunsetopt zle_bracketed_paste 2>/dev/null\n")
            server = runner.IsolatedServer(args.output, args)
            cleanup.callback(server.stop)
            server.wait_ready(90)
            proxy = TerminalProxy(server.url)
            cleanup.callback(proxy.stop)
            env = runner.native_env(state, proxy.url, "openchamber-dark")

            # A private IBus with libpinyin for composition, as native_platform.py does.
            def start(name, argv, child_env):
                log = cleanup.enter_context((args.output / f"{name}.log").open("w"))
                process = subprocess.Popen(argv, env=child_env, stdout=log, stderr=log, start_new_session=True)
                cleanup.callback(virtual_display.stop_process, process)
                return process

            env = virtual_display.child_environment(env)
            env.update(XMODIFIERS="@im=ibus", GTK_IM_MODULE="ibus", QT_IM_MODULE="ibus", GSETTINGS_BACKEND="keyfile")
            for key in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME"):
                (Path(env[key]) / "ibus/libpinyin").mkdir(parents=True, exist_ok=True)
            for key, value in (("preload-engines", "['libpinyin', 'xkb:us::eng']"),
                               ("engines-order", "['xkb:us::eng', 'libpinyin']"), ("enable-by-default", "true")):
                command(env, "gsettings", "set", "org.freedesktop.ibus.general", key, value)
            command(env, "gsettings", "set", "com.github.libpinyin.ibus-libpinyin.libpinyin", "lua-extension", "false")
            start("ibus", ["ibus-daemon", "--xim", "--panel=disable", "--config=/usr/lib/ibus/ibus-dconf",
                           "--emoji-extension=disable", "--cache=none"], env)
            wait_for("IBus did not register libpinyin", lambda: "libpinyin" in command(env, "ibus", "list-engine"), timeout=90)
            env["IBUS_ADDRESS"] = command(env, "ibus", "address").strip()
            start("libpinyin", ["/usr/lib/ibus-libpinyin/ibus-engine-libpinyin", "--ibus"], env)

            def set_engine(name):
                call = ["gdbus", "call", "--address", env["IBUS_ADDRESS"], "--dest", "org.freedesktop.IBus",
                        "--object-path", "/org/freedesktop/IBus", "--method"]
                command(env, *call, "org.freedesktop.IBus.SetGlobalEngine", name)
                wait_for("IBus did not select the engine", lambda: name in command(env, *call, "org.freedesktop.IBus.GetGlobalEngine"))

            set_engine("xkb:us::eng")
            clipboard = ClipboardProbe()
            set_accessibility(True)

            def request(path, method="GET", body=None):
                data = json.dumps(body).encode() if body is not None else None
                headers = {"Content-Type": "application/json"} if data is not None else {}
                call = urllib.request.Request(f"{server.url}{path}", data=data, method=method, headers=headers)
                with urllib.request.urlopen(call, timeout=15) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None

            roots = [entry for entry in request("/api/session?limit=200")["data"]
                     if not entry.get("parentID") and not (entry.get("time") or {}).get("archived")]
            directory = roots[0]["location"]["directory"]
            session = roots[0]["id"]
            # Each terminal changes into the run's output once, so typed
            # commands name captures by short relative paths. The session's
            # own directory is the shared seed project and stays untouched.
            capture = args.output / ".c"
            capture.mkdir()
            for name, text in (("c.py", CAPTURE_PROGRAM), ("r.py", LINE_PROGRAM), ("l.py", LOOP_PROGRAM)):
                (capture / name).write_text(text)

            def terminals():
                query = urllib.parse.urlencode({"cwd": directory})
                return request(f"/api/terminal/sessions?{query}")["sessions"]

            def launch(label):
                log = cleanup.enter_context((args.output / f"native-{label}.log").open("w"))
                client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                cleanup.callback(lambda: client.process is not None and client.process.poll() is None and client.close())
                client.launch()
                client.wait_ready(60, connected=True)
                clients.append(client)
                key = next(row["key"] for row in client.rows() if row["key"].endswith(f":session:{session}"))
                client.click(key)
                if not runner.wait_selected(client, key):
                    raise RuntimeError("the session did not open")
                return client

            def ids():
                return {node.get_accessible_id() or "" for node in clients[-1]._walk()}

            def node(accessible_id):
                return clients[-1]._find(accessible_id)

            def focus_terminal():
                clients[-1]._activate()
                x, y = clients[-1]._screen_point(node("terminal-surface"))
                command(env, "xdotool", "mousemove", str(x), str(y), "click", "1")
                time.sleep(0.2)

            def shell(text, settle=0.3):
                """Types a command line into the focused terminal as X key events."""
                command(env, "xdotool", "type", "--delay", "8", text, timeout=60)
                command(env, "xdotool", "key", "Return")
                time.sleep(settle)

            def typing_time(text):
                return 20 + SECONDS_PER_KEY * len(text)

            def read(name):
                path = capture / name
                return path.read_bytes() if path.exists() else None

            def wait_file(name, expected, timeout=60):
                wait_for(f"{name} did not receive {expected[:60]!r}", lambda: read(name) == expected, timeout=timeout)

            def start_capture(name, setup=b"", stop=0):
                line = f"python3 .c/c.py .c/{name} '{setup.hex()}' {stop}"
                shell(line, settle=1.0)
                wait_for(f"{name} did not start", lambda: read(name) is not None, timeout=typing_time(line))

            entered = []

            def enter_capture():
                """Changes the focused terminal into the run's output directory."""
                entered.append(len(entered) + 1)
                run_line(f"in-{entered[-1]}", f"cd {args.output} && touch .c/in-{entered[-1]}")

            def run_line(name, line):
                """Runs a command that writes `.c/<name>` and waits for the file."""
                shell(line)
                wait_for(f"{line!r} did not run", lambda: read(name) is not None, timeout=typing_time(line))

            def open_panel():
                clients[-1]._activate()
                command(env, "xdotool", "key", "ctrl+j")
                wait_for("the terminal panel did not open", lambda: "terminal-panel" in ids(), timeout=10)

            def new_tab():
                if "terminal-panel" not in ids():
                    open_panel()
                    wait_for("the panel offers no new tab", lambda: "terminal-new-tab" in ids(), timeout=15)
                before = len(proxy.terminal_commands("POST", "/create"))
                clients[-1].click("terminal-new-tab")
                wait_for("no terminal was created", lambda: len(proxy.terminal_commands("POST", "/create")) > before, timeout=10)
                wait_for("the terminal surface did not show", lambda: "terminal-surface" in ids(), timeout=10)
                time.sleep(1.0)
                focus_terminal()
                enter_capture()

            client = launch("first")

            def create_close():
                open_panel()
                wait_for("opening the panel started no terminal", lambda: proxy.terminal_commands("POST", "/create"), timeout=15)
                wait_for("the server lists no running terminal", lambda: len(listed(terminals(), "running")) == 1, timeout=15)
                wait_for("the terminal surface did not show", lambda: "terminal-surface" in ids(), timeout=10)
                time.sleep(1.5)
                focus_terminal()
                enter_capture()
                run_line("create.txt", "echo ok > .c/create.txt")
                wait_file("create.txt", b"ok\n")
                shell("echo ok")
                clients[-1].screenshot(args.output / "create-ok-painted.png")
                created = listed(terminals())
                clients[-1].click("terminal-tab-close-1")
                wait_for("closing sent no DELETE", lambda: proxy.terminal_commands("DELETE"), timeout=10)
                wait_for("the server still lists the closed terminal", lambda: not terminals(), timeout=15)
                return {"created": created, "deleted": [entry["path"] for entry in proxy.terminal_commands("DELETE")]}

            def unicode_input():
                new_tab()
                run_line("unicode.bin", "python3 .c/r.py .c/unicode.bin")
                time.sleep(1.0)
                # AT-SPI remaps one spare keycode per character; a character
                # read after the next remap would arrive as the wrong key, so
                # each one is given time to arrive.
                for character in UNICODE_SAMPLE:
                    clients[-1].type_text(character)
                    time.sleep(1.2)
                command(env, "xdotool", "key", "Return")
                wait_file("unicode.bin", (UNICODE_SAMPLE + "\n").encode())
                # IBus pinyin: Enter during composition commits the letters
                # without a newline; Space commits the Chinese candidate.
                run_line("ime.bin", "python3 .c/r.py .c/ime.bin")
                time.sleep(1.0)
                set_engine("libpinyin")
                command(env, "xdotool", "type", "--clearmodifiers", "nihao")
                time.sleep(3.0)
                command(env, "xdotool", "key", "Return")
                time.sleep(3.0)
                if read("ime.bin"):
                    raise RuntimeError("Enter during composition sent a line")
                command(env, "xdotool", "type", "--clearmodifiers", "nihao")
                time.sleep(3.0)
                command(env, "xdotool", "key", "space")
                time.sleep(3.0)
                set_engine("xkb:us::eng")
                command(env, "xdotool", "key", "Return")
                wait_file("ime.bin", "nihao你好\n".encode())
                clients[-1].screenshot(args.output / "unicode-input.png")
                return {"unicode": UNICODE_SAMPLE, "ime": "nihao你好"}

            def keys():
                for application, name in ((False, "keys-normal.bin"), (True, "keys-application.bin")):
                    start_capture(name, b"\x1b[?1h" if application else b"")
                    for key, *_ in KEYS:
                        command(env, "xdotool", "key", key)
                        time.sleep(0.05)
                    command(env, "xdotool", "key", "q")
                    wait_file(name, expected_keys(application))
                    time.sleep(0.5)
                return {"keys": [key for key, *_ in KEYS]}

            def resize():
                run_line("size-1.txt", "stty size > .c/size-1.txt")
                before = len(proxy.terminal_commands("POST", "/resize"))
                window = clients[-1]._window()
                command(env, "xdotool", "windowsize", window, "1280", "600")
                time.sleep(2.0)
                resizes = proxy.terminal_commands("POST", "/resize")[before:]
                if len(resizes) != 1:
                    raise RuntimeError(f"one window resize sent {len(resizes)} PTY resizes")
                run_line("size-2.txt", "stty size > .c/size-2.txt")
                first = (read("size-1.txt") or b"").split()
                second = (read("size-2.txt") or b"").split()
                body = resizes[0]["body"]
                if [int(value) for value in second] != [body["rows"], body["cols"]]:
                    raise RuntimeError(f"stty reports {second}, the PTY got {body}")
                if not int(second[0]) < int(first[0]) or second[1] != first[1]:
                    raise RuntimeError(f"a shorter window went from {first} to {second}")
                command(env, "xdotool", "windowsize", window, "1280", "800")
                time.sleep(1.0)
                clients[-1]._resize_to_requested(10)
                time.sleep(1.0)
                return {"before": [int(value) for value in first], "after": body}

            def paste_once(name, text, dispatch, expected):
                start_capture(name, b"\x1b[?2004h" if expected.startswith(b"\x1b[200~") else b"")
                clipboard.paste(text, dispatch, lambda: read(name) == expected, timeout_ms=10000)
                time.sleep(0.5)
                if read(name) != expected:
                    raise RuntimeError(f"{name} received the paste more than once: {read(name)!r}")
                focus_terminal()
                command(env, "xdotool", "key", "q")
                wait_file(name, expected + b"q")

            def shortcut():
                clients[-1]._activate()
                command(env, "xdotool", "key", "ctrl+shift+v")

            def menu():
                clients[-1]._activate()
                x, y = clients[-1]._screen_point(node("terminal-surface"))
                command(env, "xdotool", "mousemove", str(x), str(y), "click", "3")
                wait_for("the context menu did not open", lambda: "terminal-menu-paste" in ids(), timeout=5)
                clients[-1].click("terminal-menu-paste")

            def bracketed_paste():
                text = "echo one\necho two"
                bracketed = b"\x1b[200~echo one\necho two\x1b[201~"
                plain = b"echo one\recho two"
                paste_once("paste-shortcut-bracketed.bin", text, shortcut, bracketed)
                paste_once("paste-menu-bracketed.bin", text, menu, bracketed)
                paste_once("paste-shortcut-plain.bin", text, shortcut, plain)
                paste_once("paste-menu-plain.bin", text, menu, plain)
                return "markers only with ?2004h; one delivery per paste through the shortcut and the menu"

            def large_paste():
                lines = []
                while sum(len(line) for line in lines) < 200 * 1024:
                    lines.append(f"{len(lines):06d} abcdefghijklmnoprstuvwxyz 0123456789\n")
                text = "".join(lines)
                expected = text.replace("\n", "\r").encode()
                sockets = len(proxy.upgrades)
                start_capture("large-paste.bin", b"", len(expected))
                clipboard.paste(text, shortcut, lambda: read("large-paste.bin") == expected, timeout_ms=60000)
                if len(proxy.upgrades) != sockets or proxy.open_terminal_sockets() != 1:
                    raise RuntimeError("the paste closed the terminal socket")
                time.sleep(0.5)
                run_line("after-paste.txt", "echo after > .c/after-paste.txt")
                wait_file("after-paste.txt", b"after\n")
                return {"bytes": len(expected), "socketsOpened": len(proxy.upgrades)}

            def scrollback():
                shell("seq 1 20000", settle=3.0)

                def position():
                    value = node("terminal-scrollbar").get_value_iface()
                    if value is None:
                        raise LookupError("the scrollbar exposes no value")
                    return value.get_maximum_value(), value.get_current_value()

                wait_for("the output did not fill the scrollback", lambda: position()[0] >= 10_000, timeout=60)
                time.sleep(1.0)
                maximum, current = position()
                if current != maximum:
                    raise RuntimeError(f"scrollback max {maximum}, position {current}: not at the live screen")
                focus_terminal()
                command(env, "xdotool", "click", "--repeat", "20", "--delay", "10", "4")
                wait_for("the wheel did not scroll into history", lambda: position()[1] < maximum, timeout=30)
                clients[-1].screenshot(args.output / "scrollback-history.png")
                shell("clear", settle=0.5)
                return {"scrollbackRows": maximum}

            def reconnect():
                # The socket is cut only once the program runs: input typed
                # while it is down is reported as failed, never replayed.
                run_line("loop.txt", "python3 .c/l.py .c/loop.txt")
                attempts_before = len(proxy.upgrades)
                severed = proxy.sever()
                if severed != 1:
                    raise RuntimeError(f"expected one terminal socket, severed {severed}")
                wait_for("no reconnecting state was shown", lambda: "terminal-status" in ids(), timeout=10)
                status = node("terminal-status").get_name()
                time.sleep(9.0)
                attempts = proxy.upgrades[attempts_before:]
                pacing = backoff_within_contract(attempts)
                clients[-1].screenshot(args.output / "reconnecting.png")
                proxy.release()
                wait_for("the socket did not come back", lambda: proxy.open_terminal_sockets() == 1, timeout=20)
                wait_for("the reconnecting state stayed", lambda: "terminal-status" not in ids(), timeout=10)
                wait_file("loop.txt", b"started\ndone\n", timeout=30)
                run_line("reconnected.txt", "echo reconnected > .c/reconnected.txt")
                wait_file("reconnected.txt", b"reconnected\n")
                clients[-1].screenshot(args.output / "reconnected.png")
                gaps = [round(later - earlier, 2) for earlier, later in zip(attempts, attempts[1:])]
                if not pacing:
                    raise RuntimeError(f"reconnect attempts were not paced by the contract: {gaps}")
                return {"status": status, "attemptGaps": gaps}

            def exit_kept():
                new_tab()
                before = listed(terminals(), "running")
                shell("exit 3", settle=1.5)
                wait_for("the exit was not shown", lambda: "terminal-status" in ids(), timeout=10)
                exited = [entry for entry in terminals() if entry["status"] == "exited"]
                if not exited:
                    raise RuntimeError("the server shows no exited terminal")
                if proxy.terminal_commands("DELETE")[1:]:
                    raise RuntimeError("closing on exit was off, but the tab was closed")
                clients[-1].screenshot(args.output / "exit-kept.png")
                return {"status": node("terminal-status").get_name(), "runningBefore": before}

            def lifetime():
                # A long-running program the next client must find again.
                tab = next(entry for entry in sorted(ids()) if entry.startswith("terminal-tab-") and "close" not in entry)
                clients[-1].click(tab)
                running = listed(terminals(), "running")
                shell("sleep 600", settle=0.5)
                clients[-1].click("window-close")
                clients[-1].process.wait(timeout=15)
                if clients[-1].process.returncode != 0:
                    raise RuntimeError("the window did not close cleanly")
                time.sleep(1.0)
                if listed(terminals(), "running") != running:
                    raise RuntimeError("closing the client changed the server's terminals")
                request("/api/config/settings?surface=desktop", "PUT", {"terminalCloseOnExit": True})
                creates = len(proxy.terminal_commands("POST", "/create"))
                launch("relaunched")
                open_panel()
                wait_for("the relaunched client did not adopt its terminals",
                         lambda: sum(entry.startswith("terminal-tab-") and "close" not in entry for entry in ids()) >= len(running),
                         timeout=15)
                if len(proxy.terminal_commands("POST", "/create")) != creates:
                    raise RuntimeError("adoption started a new terminal")
                return {"adopted": running}

            def exit_closes():
                wait_for("the adopted exited terminal stayed", lambda: not listed(terminals(), "exited"), timeout=15)
                new_tab()
                session_count = len(terminals())
                deletes = len(proxy.terminal_commands("DELETE"))
                shell("exit 3", settle=1.5)
                wait_for("closing on exit sent no DELETE", lambda: len(proxy.terminal_commands("DELETE")) > deletes, timeout=10)
                wait_for("the closed terminal stayed listed", lambda: len(terminals()) == session_count - 1, timeout=10)
                return "the tab closed and its terminal was deleted"

            def cleanup_during_output():
                focus_terminal()
                running = listed(terminals(), "running")
                shell("seq 1 3000000", settle=0.3)
                clients[-1].click("window-close")
                started = time.monotonic()
                clients[-1].process.wait(timeout=15)
                if clients[-1].process.returncode != 0:
                    raise RuntimeError("quitting during output did not exit cleanly")
                missing = set(running) - set(listed(terminals()))
                if missing:
                    raise RuntimeError(f"quitting stopped server terminals: {sorted(missing)}")
                return {"exitSeconds": round(time.monotonic() - started, 2), "serverTerminals": listed(terminals())}

            for name, run in (("terminal.create-close", create_close), ("terminal.unicode-input", unicode_input),
                              ("terminal.keys", keys), ("terminal.resize", resize),
                              ("terminal.bracketed-paste", bracketed_paste), ("terminal.large-paste", large_paste),
                              ("terminal.scrollback", scrollback), ("terminal.reconnect", reconnect),
                              ("terminal.exit-kept", exit_kept), ("terminal.lifetime", lifetime),
                              ("terminal.exit-closes", exit_closes), ("terminal.cleanup", cleanup_during_output)):
                if args.checks and name not in args.checks:
                    continue
                check(name, run)
            for client in clients:
                if client.process is not None and client.process.poll() is None:
                    client.close()
            # Every PTY the run started is the server's to stop; stop them now.
            for entry in terminals():
                request(f"/api/terminal/{urllib.parse.quote(entry['sessionId'], safe='')}", "DELETE")
    except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration, subprocess.SubprocessError,
            http.client.HTTPException) as error:
        traceback.print_exc()
        results.append({"check": "setup", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({
            "binary": binary_info(args.binary), "display": "owned-xvfb", "scale": args.scale, "checks": results,
            "substitutions": [item for client in clients for item in client.substitutions],
        }, indent=2))
    passed = bool(results) and all(result["passed"] for result in results)
    print(f"{'passed' if passed else 'failed'}: {args.output / 'report.json'}", flush=True)
    return 0 if passed else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--root", type=Path, default=runner.REPO_ROOT / "tmp/parity")
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--checks", nargs="*", help="run only these checks, in their usual order")
    args = parser.parse_args()
    args.root = args.root.resolve()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"native-terminal-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists() or not (args.root / "seed").is_dir():
        parser.error("choose a new output directory and an existing parity seed")
    if " " in str(args.output):
        parser.error("the output path is typed into a shell; choose one without spaces")
    # IBus binds a socket under state/cache/ibus; Unix socket paths stop at 108 bytes.
    if len(str(args.output / "state" / "cache" / "ibus" / "dbus-XXXXXXXX")) > 107:
        parser.error("the output path is too long for the IBus socket; choose a shorter one")
    return qualify(args)


if __name__ == "__main__":
    raise SystemExit(main())
