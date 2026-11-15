"""Qualify the native Files surface against an owned real engine.

Starts an isolated server whose project is the Files and Git fixture
repository (`isolated-server.mjs --git-fixture`), opens the Files surface from
the panel rail and drives it through AT-SPI and real keyboard input on a
private display: tree navigation and gitignored entries, name and content
search with an invalid pattern, edit and save of a CRLF file, autosave, an
external change to a clean file, an external change while the draft is
dirty, a deleted and recreated open file, a rename onto an existing name and
a large file that opens as a preview. Every check reads the outcome on disk
too.
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

from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, editable_text, wait_for
import runner
import virtual_display

LARGE_CHARS = 300_000


class GitFixtureServer(runner.IsolatedServer):
    """`isolated-server.mjs --git-fixture` on a fresh root: no seed, the
    project is the fixture repository."""

    def __init__(self, root: Path, args: argparse.Namespace):
        self.root = root
        self.port = runner.free_port()
        self.fixture_port = runner.free_port()
        command_line = [
            "node", str(runner.ISOLATED_SERVER), "--root", str(root),
            "--port", str(self.port), "--fixture-port", str(self.fixture_port),
            "--dist", str(args.dist), "--opencode-binary", str(args.opencode_binary), "--git-fixture",
        ]
        self.log = open(root / "isolated-server.log", "w")
        self.process = subprocess.Popen(command_line, cwd=runner.REPO_ROOT, stdout=self.log,
                                        stderr=subprocess.STDOUT, start_new_session=True)

    @property
    def project(self) -> Path:
        return self.root / "project"


def git(project: Path, *args: str) -> str:
    """Git in the fixture with no system or global configuration."""
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(project.parent), "GIT_CONFIG_NOSYSTEM": "1",
           "GIT_CONFIG_GLOBAL": "/dev/null"}
    result = subprocess.run(["git", *args], cwd=project, env=env, capture_output=True, text=True, timeout=60)
    if result.returncode:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def porcelain(project: Path) -> list[str]:
    """`git status --porcelain` lines, sorted."""
    return sorted(line for line in git(project, "status", "--porcelain").splitlines() if line)


class Session:
    """A fixture server, the native client connected to it and the AT-SPI
    helpers both Files and Git scenarios use."""

    def __init__(self, args, cleanup: ExitStack, before_launch=None):
        self.args = args
        self.server = GitFixtureServer(args.output, args)
        cleanup.callback(self.server.stop)
        self.server.wait_ready(90)
        self.project = self.server.project
        if before_launch:
            before_launch(self)
        proxy = getattr(self, "proxy", None)
        if proxy is not None:
            cleanup.callback(proxy.stop)
        state = args.output / "native-state"
        for directory in runner.state_dirs(state).values():
            Path(directory).mkdir(parents=True, exist_ok=True)
        self.env = runner.native_env(state, proxy.url if proxy is not None else self.server.url, "openchamber-dark")
        self.env.update(getattr(args, "extra_env", {}))
        log = cleanup.enter_context((args.output / "native.log").open("w"))
        self.client = NativeClient(args.binary, self.env, args.scale, 1440, 900, stderr=log)
        cleanup.callback(self.client.close)
        self.client.launch()
        self.client.wait_ready(90, connected=True)

    def ids(self) -> set[str]:
        return {node.get_accessible_id() or "" for node in self.client._walk()}

    def has(self, accessible_id: str) -> bool:
        return accessible_id in self.ids()

    def label(self, accessible_id: str) -> str:
        try:
            return self.client._find(accessible_id).get_name() or ""
        except LookupError:
            return ""

    def wait_id(self, accessible_id: str, timeout=20):
        wait_for(f"{accessible_id} did not appear", lambda: self.has(accessible_id), timeout=timeout)

    def wait_gone(self, accessible_id: str, timeout=20):
        wait_for(f"{accessible_id} did not go away", lambda: not self.has(accessible_id), timeout=timeout)

    def wait_label(self, accessible_id: str, check, description: str, timeout=20):
        wait_for(description, lambda: check(self.label(accessible_id)), timeout=timeout)

    def has_state(self, accessible_id: str, state_name: str) -> bool:
        from native_adapter import _atspi
        state = getattr(_atspi().StateType, state_name)
        return self.client._find(accessible_id).get_state_set().contains(state)

    def keys(self, *combos: str):
        self.client._activate()
        for combo in combos:
            command(self.env, "xdotool", "key", "--clearmodifiers", combo)
            time.sleep(0.05)

    def type(self, text: str):
        self.client._activate()
        command(self.env, "xdotool", "type", "--delay", "25", text)

    def node_named(self, name: str, roles=("text", "entry", "editbar", "document text", "terminal")):
        matches = [node for node in self.client._walk()
                   if (node.get_name() or "") == name and node.get_role_name() in roles]
        if len(matches) != 1:
            raise LookupError(f"{len(matches)} editable nodes are named {name!r}")
        return matches[0]

    def editor_text(self, name: str) -> str:
        return editable_text(self.node_named(name))

    def click_node(self, node):
        self.client._activate()
        x, y = self.client._screen_point(node)
        command(self.env, "xdotool", "mousemove", str(x), str(y), "click", "1")

    def screenshot(self, name: str):
        self.client.screenshot(self.args.output / f"{name}.png")

    def choose_project(self):
        self.client.click("choose-project")
        target = f"project-option:{self.project}"
        self.wait_id(target)
        self.client.click(target)
        self.wait_gone(target)

    def focus_composer(self):
        entries = [node for node in self.client._walk() if node.get_role_name() == "entry"]
        if len(entries) != 1:
            raise LookupError(f"expected the composer as the sole entry, found {len(entries)}")
        self.click_node(entries[0])
        wait_for("the composer did not gain focus", lambda: self.client.focused_text() is not None)

    def request(self, route: str, body=None, method=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.server.url + route, data=data,
                                         headers={"Content-Type": "application/json"}, method=method)
        with urllib.request.urlopen(request, timeout=30) as answer:
            return json.load(answer) if answer.status != 204 else None


def _file_link_response(session, prior_sessions, prompt, uri):
    """The fresh fixture turn's exact assistant ID from V2 message records."""
    from urllib.parse import quote

    records = session.request("/api/session?limit=100")["data"]
    if len(records) >= 100:
        raise LookupError("the fixture session page is incomplete")
    created = [record for record in records if record["id"] not in prior_sessions]
    if len(created) != 1:
        raise LookupError(f"{len(created)} new file-link sessions")
    if created[0]["location"]["directory"] != str(session.project):
        raise LookupError("the file-link session has the wrong project")
    session_id = quote(created[0]["id"], safe="")
    messages = session.request(f"/api/session/{session_id}/message?limit=100")["data"]
    if len(messages) >= 100:
        raise LookupError("the fixture message page is incomplete")
    users = [record for record in messages if record["type"] == "user"]
    replies = [record for record in messages if record["type"] == "assistant"]
    if len(users) != 1 or users[0]["text"] != prompt or len(replies) != 1:
        raise LookupError("the file-link turn is missing or ambiguous")
    text = "".join(item["text"] for item in replies[0]["content"] if item["type"] == "text")
    if text != f"[Outside file]({uri})":
        raise LookupError("the assistant has not returned the exact fixture link")
    identity = replies[0]["id"]
    if not isinstance(identity, str) or not identity:
        raise LookupError("the assistant response has no message ID")
    return f"message:{identity}"


def _file_link_fragments(client, response_id, uri):
    """Fresh, scoped fragment nodes and their supplied physical WINDOW bounds.

    The geometry producer owns shaping and the real hit predicate. AT-SPI
    cannot establish either fact from a rectangle alone.
    """
    from native_adapter import _atspi

    api = _atspi()
    client._app.clear_cache()

    def visible(node):
        states = node.get_state_set()
        return (not states.contains(api.StateType.DEFUNCT)
                and states.contains(api.StateType.VISIBLE)
                and states.contains(api.StateType.SHOWING))

    def rect(node):
        box = node.get_extents(api.CoordType.WINDOW)
        values = (box.x, box.y, box.width, box.height)
        if any(type(value) is not int for value in values) or min(values[2:]) < 2:
            raise LookupError("a file-link node has no interior physical-pixel target")
        return values

    def contained(inner, outer):
        x, y, width, height = inner
        left, top, wide, high = outer
        return left <= x and top <= y and x + width <= left + wide and y + height <= top + high

    response = client._find(response_id)
    if (not response_id.startswith("message:") or response.get_role_name() != "paragraph"
            or (response.get_name() or "").strip() != "Outside file" or not visible(response)):
        raise LookupError("the exact assistant response is missing or no longer visible")
    clips = [client._find("workspace"), client._find("transcript"), response]
    if any("Component" not in node.get_interfaces() or not visible(node) for node in clips):
        raise LookupError("the response viewport has no current visible component geometry")
    clip_rects = [rect(node) for node in clips]
    stack, links = [response], []
    while stack:
        node = stack.pop()
        if node is None:
            raise LookupError("the assistant subtree changed during traversal")
        if node != response and (node.get_accessible_id() or "").startswith("message:"):
            continue
        if node.get_role_name() == "link" and node.get_name() == "Outside file":
            links.append(node)
        stack.extend(node.get_child_at_index(index) for index in reversed(range(node.get_child_count())))
    if len(links) != 1:
        raise LookupError(f"{len(links)} semantic Outside file links in the exact response")
    link = links[0]
    if not visible(link) or not {"Hyperlink", "Action"}.issubset(link.get_interfaces()):
        raise LookupError("the file link lacks current visibility, destination or action")
    hyperlink = link.get_hyperlink()
    action = link.get_action_iface()
    if (hyperlink is None or not hyperlink.is_valid() or hyperlink.get_n_anchors() != 1
            or hyperlink.get_uri(0) != uri):
        raise LookupError("the file link has no valid exact outside-file destination")
    if action is None or "click" not in [action.get_action_name(index) for index in range(action.get_n_actions())]:
        raise LookupError("the file link has no advertised click action")
    fragments, seen = [], set()
    for index in range(link.get_child_count()):
        node = link.get_child_at_index(index)
        if node is None:
            raise LookupError("the file-link children changed during traversal")
        identity = node.get_accessible_id() or ""
        if not identity.startswith("markdown-link-fragment:"):
            continue
        if identity in seen:
            raise LookupError("the file link has ambiguous fragment identities")
        seen.add(identity)
        states = node.get_state_set()
        interfaces = set(node.get_interfaces())
        if (node.get_role_name() != "label" or "Component" not in interfaces
                or interfaces.intersection({"Action", "EditableText", "Hyperlink"})
                or not visible(node) or states.contains(api.StateType.EDITABLE)
                or states.contains(api.StateType.FOCUSABLE) or states.contains(api.StateType.FOCUSED)):
            continue
        try:
            bounds = rect(node)
        except LookupError:
            continue
        if all(contained(bounds, clip) for clip in clip_rects):
            fragments.append((identity, node, bounds))
    if not fragments:
        raise LookupError("the semantic link has no eligible current visible passive Label fragment")
    return fragments


def _file_link_target(client, response_id, uri):
    """Re-resolve a supplied stable key and check the unchanged adapter's point."""
    identity, node, bounds = _file_link_fragments(client, response_id, uri)[0]
    point = client._screen_point(node)
    geometry = dict(line.split("=", 1) for line in subprocess.run(
        ["xdotool", "getwindowgeometry", "--shell", client._window()],
        capture_output=True, text=True, check=True, timeout=10,
    ).stdout.split())
    current = [entry for entry in _file_link_fragments(client, response_id, uri) if entry[0] == identity]
    if len(current) != 1 or current[0][2] != bounds:
        raise LookupError("the chosen file-link fragment changed before pointer input")
    x, y, width, height = current[0][2]
    local = (point[0] - int(geometry["X"]), point[1] - int(geometry["Y"]))
    if (local != (x + width // 2, y + height // 2)
            or not (x < local[0] < x + width and y < local[1] < y + height)):
        raise LookupError("the adapter's emitted centre misses the current fragment interior")
    return current[0][1]

def run_checks(args, steps_for, before_launch=None) -> int:
    """Runs `steps_for(session)` checks in order and writes `report.json`."""
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()
    session = None
    try:
        with ExitStack() as cleanup:
            set_accessibility(True)
            session = Session(args, cleanup, before_launch)
            selected = steps_for(session)
            requested = set(filter(None, getattr(args, "checks", "").split(",")))
            if requested:
                missing = requested - {name for name, _ in selected}
                if missing:
                    raise ValueError(f"unknown checks: {sorted(missing)}")
                selected = [(name, step) for name, step in selected if name in requested]
            for name, step in selected:
                try:
                    step()
                    results.append({"check": name, "passed": True})
                    if session.client.process.poll() is None:
                        session.screenshot(name)
                except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, AssertionError,
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
            if session.client.process.poll() is None:
                session.client.request_window_close()
                session.client.process.wait(timeout=120)
            results.append({"check": "close", "passed": session.client.process.returncode == 0 and time.monotonic() - started < 15,
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


def file_steps(session: Session):
    project = session.project
    row = lambda relative: f"files-row:{project}/{relative}"  # noqa: E731

    def open_panel():
        session.client.click("rail-files")
        session.wait_id("files-panel")
        session.wait_id(row("README.md"))
        listed = session.ids()
        for entry in ("src", "docs", "notes", "nested", ".gitignore"):
            if row(entry) not in listed:
                raise LookupError(f"{entry} is not listed")
        if row("ignored") in listed:
            raise RuntimeError("a gitignored folder is listed")
        node = session.client._find(row("src"))
        if node.get_role_name() != "tree item":
            raise RuntimeError(f"tree rows are {node.get_role_name()!r}")

    def gitignored():
        session.client.click("files-show-gitignored")
        session.wait_id(row("ignored"))
        session.client.click("files-show-gitignored")
        session.wait_gone(row("ignored"))

    def keyboard():
        session.client.click(row("src"))
        session.wait_id(row("src/app.txt"))
        if not session.has_state(row("src"), "EXPANDED"):
            raise RuntimeError("an open folder does not report expanded")
        session.keys("Left")
        session.wait_gone(row("src/app.txt"))
        session.keys("Right")
        session.wait_id(row("src/app.txt"))
        session.keys("Down", "Return")
        wait_for("Enter did not open the file", lambda: session.editor_text("app.txt").startswith("line 1 changed"))

    def search_names():
        session.client.click("files-search")
        session.type("crlf")
        session.wait_id(f"files-result:{project}/docs/crlf.txt")
        session.client.click("files-search-clear")
        session.wait_id(row("README.md"))

    def search_contents():
        session.client.click("files-search-contents")
        session.client.click("files-search")
        session.type("line 3")
        session.wait_id(f"files-match:{project}/src/app.txt:3")
        session.client.click("files-search-regex")
        session.client.click("files-search")
        session.keys("ctrl+a")
        session.type("(")
        session.wait_label("files-search-state", lambda text: text == "This regular expression is not valid.",
                           "the invalid pattern was not reported")
        session.client.click("files-search-clear")
        session.client.click("files-search-regex")
        session.client.click("files-search-names")
        time.sleep(1)
        leftover = subprocess.run(["pgrep", "-f", f"rg .*{project}"], capture_output=True, text=True).stdout.split()
        if leftover:
            raise RuntimeError(f"ripgrep still runs: {leftover}")

    def edit_save():
        session.client.click(row("docs"))
        session.wait_id(row("docs/crlf.txt"))
        session.client.click(row("docs/crlf.txt"))
        wait_for("the CRLF file did not load", lambda: session.editor_text("crlf.txt") == "first\nsecond\nthird\n")
        session.click_node(session.node_named("crlf.txt"))
        session.keys("ctrl+End")
        session.type("fourth")
        session.wait_label("files-save-status", lambda text: text in ("Unsaved changes", "Saving...", "Saved"),
                           "the draft is not marked")
        session.keys("ctrl+s")
        wait_for("the save did not reach the disk",
                 lambda: (project / "docs/crlf.txt").read_bytes() == b"first\r\nsecond\r\nthird\r\nfourth")
        session.wait_label("files-save-status", lambda text: text == "Saved", "the saved state is not shown")

    def autosave():
        session.type(" auto")
        wait_for("autosave did not write", lambda: (project / "docs/crlf.txt").read_bytes()
                 == b"first\r\nsecond\r\nthird\r\nfourth auto", timeout=10)

    def external_clean():
        session.client.click(row("README.md"))
        wait_for("README did not load", lambda: session.editor_text("README.md").startswith("# Fixture"))
        (project / "README.md").write_text("# Changed outside\n")
        wait_for("the clean file did not take the change", lambda: session.editor_text("README.md")
                 == "# Changed outside\n", timeout=15)

    def external_dirty():
        session.click_node(session.node_named("README.md"))
        session.keys("ctrl+End")
        session.type("mine")
        (project / "README.md").write_text("theirs\n")
        time.sleep(0.5)
        if session.editor_text("README.md") != "# Changed outside\nmine":
            raise RuntimeError("the dirty draft was replaced")
        wait_for("the draft was not saved over the external change",
                 lambda: (project / "README.md").read_text() == "# Changed outside\nmine", timeout=10)
        time.sleep(3)
        if session.editor_text("README.md") != "# Changed outside\nmine":
            raise RuntimeError("the saved draft was replaced")

    def deleted_open():
        (project / "README.md").unlink()
        session.wait_id("files-error", timeout=15)
        (project / "README.md").write_text("# Back\n")
        wait_for("the recreated file did not load", lambda: session.editor_text("README.md") == "# Back\n",
                 timeout=15)

    def rename_conflict():
        session.client.click("files-refresh")
        session.wait_id(row("README.md"))
        session.client.click(row("notes"))
        session.wait_id(row("README.md"))
        session.client.click(row("README.md"))
        session.keys("F2")
        time.sleep(0.3)
        session.keys("ctrl+a")
        session.type("src")
        session.keys("Return")
        session.wait_label("files-notice", lambda text: "already has a file or folder named src" in text,
                           "the name conflict was not reported")
        if not (project / "README.md").exists() or not (project / "src/app.txt").exists():
            raise RuntimeError("a rename onto an existing name replaced something")

    def large():
        line = "x" * 59 + "\n"
        (project / "large.txt").write_text(line * (LARGE_CHARS // len(line)) + "y" * (LARGE_CHARS % len(line)))
        session.client.click("files-refresh")
        session.wait_id(row("large.txt"))
        session.client.click(row("large.txt"))
        session.wait_id("files-preview", timeout=30)
        wait_for("the large file did not open as a preview", lambda: session.has_state("files-preview", "CHECKED")
                 or session.has_state("files-preview", "PRESSED"), timeout=30)
        session.client.click("files-preview")
        wait_for("editing does not keep the whole file", lambda: len(session.editor_text("large.txt")) == LARGE_CHARS,
                 timeout=30)

    def dirty_switch():
        target = project / "switch.txt"
        target.write_text("saved\n")
        session.client.click("files-refresh")
        session.wait_id(row("switch.txt"))
        session.client.click(row("switch.txt"))
        wait_for("switch file did not load", lambda: session.editor_text("switch.txt") == "saved\n")
        session.click_node(session.node_named("switch.txt"))
        session.keys("ctrl+a", "BackSpace")
        session.client.click(row("README.md"))
        session.wait_id("files-unsaved")
        session.client.click("files-unsaved-cancel")
        session.wait_gone("files-unsaved")
        assert session.editor_text("switch.txt") == ""
        assert target.read_text() == "saved\n", "autosave must not erase a nonempty file"
        session.client.click(row("README.md"))
        session.wait_id("files-unsaved")
        session.client.click("files-unsaved-save")
        session.wait_gone("files-unsaved")
        wait_for("explicit Save did not write the empty draft", lambda: target.read_text() == "")
        session.client.click(row("switch.txt"))
        wait_for("empty file did not load", lambda: session.editor_text("switch.txt") == "")
        session.click_node(session.node_named("switch.txt"))
        session.type("x")
        session.client.click(row("README.md"))
        session.wait_id("files-unsaved")
        session.client.click("files-unsaved-discard")
        session.wait_gone("files-unsaved")
        wait_for("Discard did not open the next file", lambda: session.editor_text("README.md") == "# Back\n")

    def tree_memory():
        session.client.click(row("docs/crlf.txt"))
        wait_for("the remembered file did not load", lambda: session.editor_text("crlf.txt").startswith("first\n"))
        session.client.click("rail-files")
        session.wait_gone("files-panel")
        session.client.click("rail-files")
        session.wait_id(row("docs/crlf.txt"))
        assert session.has_state(row("docs"), "EXPANDED"), "hiding lost the expanded folder"
        session.client.click("window-close")
        restarted = session.client.restart("exit", 120)
        assert restarted["closed"]["exitCode"] == 0, "the remembered tree did not close cleanly"
        session.client.wait_ready(90, connected=True)
        session.client.click("rail-files")
        session.wait_id(row("docs/crlf.txt"))
        assert session.has_state(row("docs"), "EXPANDED"), "restart lost the expanded folder"
        wait_for("restart lost the selected file", lambda: session.editor_text("crlf.txt").startswith("first\n"))

    def readonly_file_link():
        outside = project.parent / "outside-proof.txt"
        outside.write_text("Outside the project. Read only.\n")
        session.client.click("rail-files")
        session.choose_project()
        session.focus_composer()
        prior_records = session.request("/api/session?limit=100")["data"]
        if len(prior_records) >= 100:
            raise LookupError("the fixture session baseline is incomplete")
        prior_sessions = {record["id"] for record in prior_records}
        prompt = f"files-link-proof: {outside.as_uri()}"
        session.type(prompt)
        wait_for("the file-link prompt was not typed", lambda: session.client.focused_text() == prompt)
        session.client.click("composer-submit")
        response_ids = []

        def response_ready():
            response_id = _file_link_response(session, prior_sessions, prompt, outside.as_uri())
            _file_link_fragments(session.client, response_id, outside.as_uri())
            response_ids[:] = [response_id]
            return True

        wait_for("the fixture never returned one semantic file-link response", response_ready, timeout=45)
        if _file_link_response(session, prior_sessions, prompt, outside.as_uri()) != response_ids[0]:
            raise LookupError("the exact assistant response changed before pointer input")
        link = _file_link_target(session.client, response_ids[0], outside.as_uri())
        session.click_node(link)
        session.wait_id("files-panel")
        wait_for("the outside file did not open", lambda: session.editor_text("outside-proof.txt")
                 == "Outside the project. Read only.\n")
        node = session.node_named("outside-proof.txt")
        session.click_node(node)
        session.keys("ctrl+a")
        session.type("must not write")
        session.keys("ctrl+s")
        time.sleep(2)
        assert session.editor_text("outside-proof.txt") == "Outside the project. Read only.\n"
        assert outside.read_text() == "Outside the project. Read only.\n", "outside read granted a write"

    return [("open-panel", open_panel), ("gitignored", gitignored), ("keyboard", keyboard),
            ("search-names", search_names), ("search-contents", search_contents), ("edit-save", edit_save),
            ("autosave", autosave), ("external-clean", external_clean), ("external-dirty", external_dirty),
            ("deleted-open", deleted_open), ("rename-conflict", rename_conflict), ("large", large),
            ("dirty-switch", dirty_switch), ("tree-memory", tree_memory), ("readonly-file-link", readonly_file_link)]


def parse_args(description: str, prefix: str):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--keep-going", action="store_true", help="run later checks after a failure")
    parser.add_argument("--checks", default="", help="comma-separated focused checks; default runs every check")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"{prefix}-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists():
        parser.error("choose a new output directory")
    return parser, args


def main():
    parser, args = parse_args(__doc__, "native-files")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1440 * args.scale), round(900 * args.scale))
    return run_checks(args, file_steps)


if __name__ == "__main__":
    sys.exit(main())
