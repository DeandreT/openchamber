"""Qualify the native source board against GitHub through a loopback fixture.

Starts the GitHub fixture (`github_fixture.py`) and an isolated server whose
project's origin is `github.com/acme/widgets` and which reaches GitHub
through the fixture's proxy (`isolated-server.mjs --github-fixture`). The
real server's GitHub provider, credential store, binding validation and
durable mutations run unchanged; only the network answers are the
fixture's. Drives the board through AT-SPI and real input on a private
display: the board before signing in, signing in through the server's
device flow, the issue list and preview, searching, an empty filter, a
comment, closing an issue while the write is held, approving a pull
request, and a failed list read and a failed comment with their recovery.
Every write is checked in the fixture's repository too.
"""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import traceback
import urllib.request

from github_fixture import Fixture
from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, wait_for
import runner
import virtual_display

ISSUE_ROW = "source-board-row:github:acme/widgets#{}"
PULL_ROW = "source-board-row:github:acme/widgets!{}"
COMMENT = "Reproduced with an empty config on Linux."


class GitHubFixtureServer(runner.IsolatedServer):
    """`isolated-server.mjs --github-fixture` on a fresh root."""

    def __init__(self, root: Path, args: argparse.Namespace, fixture: Fixture):
        self.root = root
        self.port = runner.free_port()
        self.fixture_port = runner.free_port()
        command_line = [
            "node", str(runner.ISOLATED_SERVER), "--root", str(root),
            "--port", str(self.port), "--fixture-port", str(self.fixture_port),
            "--dist", str(args.dist), "--opencode-binary", str(args.opencode_binary),
            "--github-fixture", fixture.proxy_url, str(fixture.ca),
        ]
        self.log = open(root / "isolated-server.log", "w")
        self.process = subprocess.Popen(command_line, cwd=runner.REPO_ROOT, stdout=self.log,
                                        stderr=subprocess.STDOUT, start_new_session=True)


def request(url: str, body: dict | None = None) -> dict:
    data = None if body is None else json.dumps(body).encode()
    headers = {"Accept": "application/json"}
    if data is not None:
        headers["Content-Type"] = "application/json"
    with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers,
                                                       method="GET" if data is None else "POST"), timeout=30) as answer:
        return json.load(answer)


class Session:
    def __init__(self, args, cleanup: ExitStack):
        self.args = args
        self.fixture = Fixture(args.output / "github-fixture")
        cleanup.callback(self.fixture.stop)
        self.server = GitHubFixtureServer(args.output, args, self.fixture)
        cleanup.callback(self.server.stop)
        self.server.wait_ready(90)
        state = args.output / "native-state"
        for directory in runner.state_dirs(state).values():
            Path(directory).mkdir(parents=True, exist_ok=True)
        self.env = runner.native_env(state, self.server.url, "openchamber-dark")
        log = cleanup.enter_context((args.output / "native.log").open("w"))
        self.client = NativeClient(args.binary, self.env, args.scale, 1440, 900, stderr=log)
        cleanup.callback(self.client.close)
        self.client.launch()
        self.client.wait_ready(90, connected=True)

    def control(self, path: str) -> dict:
        return request(f"{self.fixture.proxy_url}/__fixture/{path}")

    def repository(self) -> dict:
        return self.control("state")

    def ids(self) -> set[str]:
        return {node.get_accessible_id() or "" for node in self.client._walk()}

    def has(self, accessible_id: str) -> bool:
        return accessible_id in self.ids()

    def label(self, accessible_id: str) -> str:
        try:
            return self.client._find(accessible_id).get_name() or ""
        except LookupError:
            return ""

    def role(self, accessible_id: str) -> str:
        try:
            return self.client._find(accessible_id).get_role_name() or ""
        except LookupError:
            return ""

    def wait_id(self, accessible_id: str, timeout=30):
        wait_for(f"{accessible_id} did not appear", lambda: self.has(accessible_id), timeout=timeout)

    def wait_gone(self, accessible_id: str, timeout=30):
        wait_for(f"{accessible_id} did not go away", lambda: not self.has(accessible_id), timeout=timeout)

    def wait_label(self, accessible_id: str, expected: str, timeout=30):
        wait_for(f"{accessible_id} never read {expected!r} (last {self.label(accessible_id)!r})",
                 lambda: expected in self.label(accessible_id), timeout=timeout)

    def selected(self, accessible_id: str) -> bool:
        from native_adapter import _atspi
        try:
            states = self.client._find(accessible_id).get_state_set()
        except LookupError:
            return False
        return states.contains(_atspi().StateType.SELECTED)

    def type(self, text: str):
        self.client._activate()
        command(self.env, "xdotool", "type", "--delay", "25", text)

    def click_node(self, accessible_id: str):
        self.client._activate()
        x, y = self.client._screen_point(self.client._find(accessible_id))
        command(self.env, "xdotool", "mousemove", str(x), str(y), "click", "1")

    def screenshot(self, name: str):
        self.client.screenshot(self.args.output / f"{name}.png")


def open_page(session: Session, line: str):
    """Chooses a page's line in the sidebar's More menu, which toggles it."""
    session.client.click("sidebar-more")
    session.wait_id(line)
    session.client.click(line)


def board_steps(session: Session):
    client = session.client
    server = session.server.url

    def unauthenticated():
        open_page(session, "source-board")
        session.wait_id("source-board-page")
        session.wait_label("source-board-state", "Connect GitHub")
        assert session.repository()["writes"] == []

    def sign_in():
        started = request(f"{server}/api/source-control/github/auth/start?instance=github.com", {})
        completed = request(f"{server}/api/source-control/github/auth/complete?instance=github.com",
                            {"flowId": started["flowId"]})
        if completed.get("connected") is not True:
            raise RuntimeError(f"the device flow did not connect: {completed}")
        # The board asks again when it opens on a project it could not read.
        open_page(session, "source-board")
        session.wait_gone("source-board-page")
        open_page(session, "source-board")

    def selected():
        session.wait_id(ISSUE_ROW.format(1))
        session.wait_id(ISSUE_ROW.format(2))
        if session.has(PULL_ROW.format(3)):
            raise RuntimeError("the issue tab lists a pull request")
        wait_for("the newest issue is not highlighted", lambda: session.selected(ISSUE_ROW.format(1)), timeout=15)
        session.wait_label("source-board-title", "Crash on startup when the config is empty")
        session.wait_label("source-board-comment:0", "Still happens on 2.6.")

    def search():
        session.type("dark")
        session.wait_gone(ISSUE_ROW.format(1))
        session.wait_id(ISSUE_ROW.format(2))
        session.wait_label("source-board-title", "Add a dark theme")
        client.click("source-board-search-clear")
        session.wait_id(ISSUE_ROW.format(1))

    def empty():
        client.click("source-board-filter")
        session.wait_id("source-board-filter-state:closed")
        client.click("source-board-filter-state:closed")
        session.wait_label("source-board-list-state", "No open issues.")
        session.wait_label("source-board-preview-empty", "Pick an item")
        # The menu stays open so state and people combine.
        client.click("source-board-filter-state:open")
        session.wait_id(ISSUE_ROW.format(1))
        client.click("source-board-filter")
        session.wait_gone("source-board-menu")

    def comment():
        client.click(ISSUE_ROW.format(1))
        session.wait_label("source-board-title", "Crash on startup")
        session.click_node("source-board-reply")
        session.type(COMMENT)
        client.click("source-board-comment")
        session.wait_label("source-board-notice", "Commented on #1")
        session.wait_label("source-board-comment:1", COMMENT)
        writes = session.repository()["writes"]
        if writes[-1:] != [{"write": "comment", "number": 1, "body": COMMENT}]:
            raise RuntimeError(f"the fixture did not record the comment: {writes}")

    def pending_close():
        session.control("hold?operation=state")
        actionable = session.role("source-board-state")
        client.click("source-board-state")
        session.wait_id("source-board-state-change")
        client.click("source-board-state-change")
        # While the write is out the state pill is no button.
        wait_for("the close never showed as pending",
                 lambda: session.role("source-board-state") not in ("", actionable), timeout=15)
        session.screenshot("pending")
        session.control("release")
        session.wait_label("source-board-notice", "Closed #1")
        # The open list no longer holds the closed issue.
        session.wait_gone(ISSUE_ROW.format(1))
        if session.repository()["items"]["1"]["state"] != "CLOSED":
            raise RuntimeError("the fixture issue is still open")

    def review():
        client.click("source-board-kind:pull")
        session.wait_id(PULL_ROW.format(3))
        client.click(PULL_ROW.format(3))
        session.wait_label("source-board-title", "Fix the crash on an empty config")
        client.click("source-board-approve")
        session.wait_label("source-board-notice", "Approved #3")
        writes = session.repository()["writes"]
        if writes[-1:] != [{"write": "review", "number": 3, "state": "APPROVED", "body": None}]:
            raise RuntimeError(f"the fixture did not record the approval: {writes}")
        session.wait_id("source-board-comment:0")

    def failed_list():
        session.control("fail?operation=search")
        # A refresh that fails keeps the rows and says so above them.
        client.click("source-board-refresh")
        session.wait_label("source-board-refresh-error", "Couldn’t refresh")
        if not session.has(PULL_ROW.format(3)):
            raise RuntimeError("a failed refresh dropped the listed rows")
        # A new list read that fails has no rows to keep.
        client.click("source-board-kind:issue")
        session.wait_label("source-board-list-state", "Couldn’t load the list")
        session.control("recover")
        client.click("source-board-list-retry")
        session.wait_id(ISSUE_ROW.format(2))
        client.click("source-board-kind:pull")
        session.wait_id(PULL_ROW.format(3))
        session.wait_gone("source-board-refresh-error")

    def failed_comment():
        comments = lambda: [write for write in session.repository()["writes"] if write["write"] == "comment"]  # noqa: E731
        before = len(comments())
        session.control("fail?operation=comment")
        session.click_node("source-board-reply")
        session.type("A note that will fail first")
        client.click("source-board-comment")
        session.wait_label("source-board-notice", "Could not comment on #3")
        if len(comments()) != before:
            raise RuntimeError(f"a failed comment was recorded: {comments()}")
        session.control("recover")
        # The text stayed in the box; the same click sends it now.
        client.click("source-board-comment")
        session.wait_label("source-board-notice", "Commented on #3")
        if len(comments()) != before + 1:
            raise RuntimeError(f"the retried comment was not recorded once: {comments()}")

    def close_board():
        open_page(session, "source-board")
        session.wait_gone("source-board-page")

    return [
        ("unauthenticated", unauthenticated),
        ("sign-in", sign_in),
        ("selected", selected),
        ("search", search),
        ("empty", empty),
        ("comment-updated", comment),
        ("close-pending-updated", pending_close),
        ("review-approved", review),
        ("failed-list-recovered", failed_list),
        ("failed-comment-recovered", failed_comment),
        ("close-board", close_board),
    ]


def run_checks(args) -> int:
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()
    try:
        with ExitStack() as cleanup:
            set_accessibility(True)
            session = Session(args, cleanup)
            for name, step in board_steps(session):
                try:
                    step()
                    results.append({"check": name, "passed": True})
                    session.screenshot(name)
                except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, AssertionError, KeyError,
                        subprocess.SubprocessError) as error:
                    traceback.print_exc()
                    results.append({"check": name, "passed": False, "error": str(error)})
                    try:
                        session.screenshot(f"{name}-failed")
                    except Exception:  # noqa: BLE001 - evidence is best effort after a failure
                        pass
                    if not args.keep_going:
                        break
            started = time.monotonic()
            session.client.click("window-close")
            session.client.process.wait(timeout=120)
            results.append({"check": "close", "passed": time.monotonic() - started < 15,
                            "seconds": round(time.monotonic() - started, 2)})
    except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, subprocess.SubprocessError) as error:
        traceback.print_exc()
        results.append({"check": "session", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({
            "binary": binary_info(args.binary), "display": "owned-xvfb", "scale": args.scale, "checks": results,
        }, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--keep-going", action="store_true", help="run later checks after a failure")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"native-source-board-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists():
        parser.error("choose a new output directory")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1440 * args.scale), round(900 * args.scale))
    return run_checks(args)


if __name__ == "__main__":
    sys.exit(main())
