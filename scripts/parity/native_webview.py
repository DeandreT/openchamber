"""Qualify the native browser surface and its WebKit host on a private display.

Starts an isolated server (the native client needs one to open a project), a
loopback page fixture this script owns, and the native client. Drives the
panel rail's Browser surface through AT-SPI and real pointer and keyboard
input, and reads outcomes from the fixture's request log, X11 captures and
`/proc`:

- page: an address typed into the toolbar loads, the tab names its host;
- blocked: `file:`, a custom scheme, the desktop's own shim scheme and a typed
  `javascript:` address never load, and the page stays where it was;
- credentials: a hostile page finds no message handler, no app cookie and no
  shim document, and no WebKit process carries the client's secrets in
  its environment or command line;
- overlay: a dialog opened over the page is painted above it, the page is
  dimmed around it, and a click on the page while it is open never reaches it;
- focus: typing goes to the page after a click in it and back to the address
  field after a click there;
- profile: a cookie the page set is kept in the browser profile under the
  client's own config directory across a restart, and nowhere else;
- teardown: closing the window ends every WebKit process it started.

Wayland is not covered: the host needs an X11 parent window.
"""

from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sys
import threading
import time
import urllib.parse

from native_files import Session, parse_args, run_checks
from native_platform import choose_app_menu_row, editable_text, wait_for
import png
import runner
import virtual_display

PAGE_COLOR = (255, 0, 255)
SECRET_MARKER = "oc-webview-secret-marker"

PAGE = """<!doctype html><html><head><meta charset="utf-8"><title>Fixture page</title>
<style>html,body{margin:0;height:100%;background:#ff00ff}
input{position:absolute;left:20px;top:20px;width:240px;height:28px;font-size:16px}</style></head>
<body><input id="field" autocomplete="off">
<script>
const report = (kind, data) => fetch('/report?kind=' + kind, { method: 'POST', body: JSON.stringify(data) });
document.cookie = 'fixture=kept; max-age=86400; path=/';
localStorage.setItem('fixture', 'kept');
const field = document.getElementById('field');
field.addEventListener('input', () => report('typed', { value: field.value }));
document.addEventListener('mousedown', (event) => report('press', { x: event.clientX, y: event.clientY }));
(async () => {
  const probe = {};
  const attempt = async (name, run) => { try { probe[name] = String(await run()); } catch (error) { probe[name] = 'threw ' + error.name; } };
  await attempt('handlers', () => typeof (window.webkit && window.webkit.messageHandlers));
  await attempt('ipc', () => typeof window.ipc);
  await attempt('cookie', () => document.cookie);
  await attempt('serverApi', async () => (await fetch(SERVER + 'api/session')).status);
  await attempt('serverHealth', async () => (await fetch(SERVER + 'health')).status);
  await attempt('shim', async () => (await fetch('openchamber-ui://app/native-frame/x')).status);
  report('probe', probe);
  if (!location.search.includes('again')) {
    tryNavigate('file:///etc/passwd');
    setTimeout(() => tryNavigate('custom-scheme://x'), 200);
    setTimeout(() => tryNavigate('openchamber-ui://app/native-frame/x'), 400);
    setTimeout(() => report('after', { href: location.href }), 1000);
  }
})();
window.tryNavigate = (target) => { try { location.href = target; } catch (error) {} };
</script></body></html>"""


class PageFixture:
    """A loopback HTTP server serving the fixture page and logging requests."""

    def __init__(self, server_url: str):
        self.requests: list[dict] = []
        fixture = self
        page = PAGE.replace("SERVER", json.dumps(server_url))

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def _record(self, body: bytes):
                url = urllib.parse.urlparse(self.path)
                fixture.requests.append({
                    "path": url.path, "query": dict(urllib.parse.parse_qsl(url.query)),
                    "cookie": self.headers.get("Cookie", ""), "body": body.decode("utf-8", "replace"),
                    "authorization": self.headers.get("Authorization"),
                })

            def do_GET(self):
                self._record(b"")
                body = page.encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                self._record(self.rfile.read(length))
                self.send_response(204)
                self.end_headers()

        self.server = ThreadingHTTPServer(("127.0.0.1", runner.free_port()), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def reports(self, kind: str) -> list[dict]:
        return [json.loads(entry["body"] or "{}") for entry in self.requests
                if entry["path"] == "/report" and entry["query"].get("kind") == kind]

    def stop(self):
        self.server.shutdown()


def process_identity(pid: int):
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        return int(stat.rsplit(") ", 1)[1].split()[19])
    except (OSError, ValueError, IndexError):
        return None


def descendants(root: int) -> dict[int, int]:
    """Live descendants of `root` by PID with their start times."""
    children: dict[int, list[int]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            parent = int((entry / "stat").read_text().rsplit(") ", 1)[1].split()[1])
        except (OSError, ValueError, IndexError):
            continue
        children.setdefault(parent, []).append(int(entry.name))
    found, queue = {}, [root]
    while queue:
        for child in children.get(queue.pop(), []):
            start = process_identity(child)
            if start is not None:
                found[child] = start
                queue.append(child)
    return found


def webkit_processes(root: int) -> dict[int, int]:
    result = {}
    for pid, start in descendants(root).items():
        try:
            name = Path(f"/proc/{pid}/comm").read_text().strip()
        except OSError:
            continue
        if name.startswith("WebKit"):
            result[pid] = start
    return result


def webview_steps(session: Session, fixture: PageFixture, state: dict):
    client = session.client

    def page_rect():
        node = client._find("browser-page")
        return client._rect(node)

    def click_page(x: float, y: float):
        """Clicks at logical `(x, y)` inside the page, in screen pixels."""
        node = client._find("browser-page")
        rect = client._rect(node)
        center_x, center_y = client._screen_point(node)
        sx = round(center_x + (x - rect[2] / 2) * client.scale)
        sy = round(center_y + (y - rect[3] / 2) * client.scale)
        from native_platform import command
        command(session.env, "xdotool", "mousemove", str(sx), str(sy), "click", "1")

    def type_address(text: str):
        session.click_node(client._find("browser-address"))
        session.keys("ctrl+a")
        session.type(text)
        session.keys("Return")

    def page():
        client.click("rail-browser")
        session.wait_id("browser-panel")
        type_address(fixture.url)
        session.wait_id("browser-page")
        wait_for("the fixture page did not load", lambda: fixture.reports("probe"), timeout=30)
        host = fixture.url.split("//", 1)[1].rstrip("/")
        wait_for("the tab does not name the page's host",
                 lambda: any(host in (node.get_name() or "") for node in client._walk()
                             if (node.get_accessible_id() or "").startswith("browser-tab-")), timeout=10)
        state["webkit"] = webkit_processes(client.process.pid)
        if not state["webkit"]:
            raise RuntimeError("no WebKit process belongs to the client")

    def blocked():
        wait_for("the page did not report after its navigations", lambda: fixture.reports("after"), timeout=15)
        after = fixture.reports("after")[-1]["href"]
        state["href_after_blocked"] = after
        if not after.startswith(fixture.url):
            raise AssertionError(f"the page left the fixture: {after}")
        loads = len([entry for entry in fixture.requests if entry["path"] == "/"])
        type_address("javascript:report('typed-script',{})")
        time.sleep(1.0)
        if fixture.reports("typed-script"):
            raise AssertionError("a typed javascript: address ran in the page")
        type_address(fixture.url + "?again=1")
        wait_for("the fixture did not load again",
                 lambda: len([entry for entry in fixture.requests if entry["path"] == "/"]) > loads, timeout=30)
        time.sleep(1.0)

    def credentials():
        probe = fixture.reports("probe")[-1]
        state["probe"] = probe
        if probe["handlers"] != "undefined" or probe["ipc"] != "undefined":
            raise AssertionError(f"the page reached a message handler: {probe}")
        if probe["cookie"] not in ("", "fixture=kept"):
            raise AssertionError(f"the page sees a cookie it did not set: {probe['cookie']}")
        # A passwordless loopback server answers any local page, in Electron's
        # panel as here; what matters is that the page holds no credential.
        state["server_api_without_credential"] = probe["serverApi"]
        if not probe["shim"].startswith("threw"):
            raise AssertionError(f"the page read a shim document: {probe['shim']}")
        if any(entry["authorization"] for entry in fixture.requests):
            raise AssertionError("a page request carried an Authorization header")
        leaks = []
        for pid in webkit_processes(client.process.pid):
            for source in ("environ", "cmdline"):
                try:
                    data = Path(f"/proc/{pid}/{source}").read_bytes()
                except OSError:
                    continue
                if SECRET_MARKER.encode() in data:
                    leaks.append({"pid": pid, "source": source})
        state["environment_leaks"] = leaks
        if leaks:
            raise AssertionError(f"a WebKit process carries the client's password variable: {leaks}")

    def overlay():
        presses = len(fixture.reports("press"))
        choose_app_menu_row(client, "about")
        session.wait_id("about-dialog")
        time.sleep(1.0)  # Kit dialogs slide in
        path = session.args.output / "overlay-open.png"
        client.screenshot(path)
        image = png.read(path)
        dialog = client._rect(client._find("about-dialog"))
        rect = page_rect()
        scale = client.scale
        inside_magenta = 0
        for y in range(int(dialog[1] * scale) + 8, int((dialog[1] + dialog[3]) * scale) - 8, 4):
            for x in range(int(dialog[0] * scale) + 8, int((dialog[0] + dialog[2]) * scale) - 8, 4):
                if image.pixel(x, y)[:3] == PAGE_COLOR:
                    inside_magenta += 1
        corner = image.pixel(int((rect[0] + rect[2] - 10) * scale), int((rect[1] + rect[3] - 10) * scale))[:3]
        state["overlay"] = {"magentaInDialog": inside_magenta, "pageCorner": corner}
        if inside_magenta:
            raise AssertionError(f"{inside_magenta} page pixels show inside the dialog")
        if corner == PAGE_COLOR or corner[0] < 40:
            raise AssertionError(f"the page is not dimmed around the dialog: {corner}")
        click_page(rect[2] - 20, rect[3] - 20)
        time.sleep(0.5)
        if len(fixture.reports("press")) != presses:
            raise AssertionError("a click reached the page under the dialog")
        session.keys("Escape")
        session.wait_gone("about-dialog")

    def focus():
        from native_platform import command
        click_page(30, 34)
        time.sleep(0.3)
        # No window activation in between: that would move X focus itself.
        command(session.env, "xdotool", "type", "--delay", "40", "abc")
        wait_for("typing did not reach the page",
                 lambda: any(report.get("value", "").endswith("abc") for report in fixture.reports("typed")))
        typed = len(fixture.reports("typed"))
        session.click_node(client._find("browser-address"))
        session.keys("ctrl+a")
        session.type("xyz")
        time.sleep(0.5)
        if len(fixture.reports("typed")) != typed:
            raise AssertionError("typing for the address field reached the page")
        text = editable_text(client._find("browser-address").get_child_at_index(0) or client._find("browser-address"))
        state["address_text"] = text
        if "xyz" not in text:
            raise AssertionError(f"the address field did not get the typing: {text!r}")

    def profile():
        config = Path(session.env["XDG_CONFIG_HOME"])
        cookies = list(config.rglob("browser-panel/cookies.sqlite"))
        if not cookies:
            raise AssertionError("the browser profile has no cookie store under the config directory")
        stray = [path for path in Path(session.env["XDG_DATA_HOME"]).rglob("*") if "webkitgtk" in str(path).lower()]
        stray += [path for path in Path(session.env["XDG_CACHE_HOME"]).rglob("*") if "webkitgtk" in str(path).lower()]
        state["stray"] = [str(path) for path in stray[:5]]
        if stray:
            raise AssertionError(f"browser data left the profile: {state['stray']}")
        started = webkit_processes(client.process.pid)
        client.click("window-close")
        client.restart("exit", 60)
        def remaining():
            return [pid for pid, start in started.items() if process_identity(pid) == start]
        deadline = time.monotonic() + 5
        while remaining() and time.monotonic() < deadline:
            time.sleep(0.1)
        left = remaining()
        state["webkit_after_close"] = left
        if left:
            raise AssertionError(f"WebKit processes outlived the window: {left}")
        client.wait_ready(90, connected=True)
        before = len([entry for entry in fixture.requests if entry["path"] == "/"])
        client.click("rail-browser")
        session.wait_id("browser-panel")
        type_address(fixture.url)
        wait_for("the fixture did not load again",
                 lambda: len([entry for entry in fixture.requests if entry["path"] == "/"]) > before, timeout=30)
        cookie = [entry["cookie"] for entry in fixture.requests if entry["path"] == "/"][-1]
        state["cookie_after_restart"] = cookie
        if "fixture=kept" not in cookie:
            raise AssertionError(f"the cookie did not survive a restart: {cookie!r}")
        state["webkit"] = webkit_processes(client.process.pid)

    return [("page", page), ("blocked", blocked), ("credentials", credentials), ("overlay", overlay),
            ("focus", focus), ("profile", profile)]


def main():
    parser, args = parse_args(__doc__, "native-webview")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1440 * args.scale), round(900 * args.scale))
    state: dict = {}
    # A development password variable must stay out of page processes.
    args.extra_env = {"OPENCHAMBER_NATIVE_PASSWORD": SECRET_MARKER}
    with ExitStack() as cleanup:
        holder: dict = {}

        def before_launch(session):
            fixture = PageFixture(session.server.url if session.server.url.endswith("/") else session.server.url + "/")
            cleanup.callback(fixture.stop)
            holder["fixture"] = fixture

        def steps(session):
            return webview_steps(session, holder["fixture"], state)

        code = run_checks(args, steps, before_launch)
        report = args.output / "report.json"
        data = json.loads(report.read_text())
        data["webview"] = state
        data["requests"] = holder["fixture"].requests if "fixture" in holder else []
        report.write_text(json.dumps(data, indent=2, default=str))
        return code


if __name__ == "__main__":
    sys.exit(main())
