"""Qualify the native scheduled tasks page and Project knowledge panel.

Starts an isolated server (`isolated-server.mjs`) whose one project is
registered and active, launches the native client on a private display and
drives both surfaces through AT-SPI and real input: an empty task list, a
daily task created in a fixed timezone, an invalid cron expression refused
before any request, pause and resume, run now opening the run's session,
delete; then notes, todos with completion, a plan written and implemented
in a new session the plan links to, and search. Every write is checked in
the server's own records.
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
import urllib.parse
import urllib.request

from native_adapter import NativeClient, _atspi, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, editable_text, wait_for
import runner
import virtual_display

TASK_NAME = "Nightly digest"
ZONE = "Europe/Kyiv"


def request(url: str, method: str = "GET", body: dict | None = None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Accept": "application/json"}
    if data is not None:
        headers["Content-Type"] = "application/json"
    with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers, method=method),
                                timeout=30) as answer:
        return json.load(answer)


class Session:
    def __init__(self, args, cleanup: ExitStack):
        self.args = args
        self.server = runner.IsolatedServer(args.output, args)
        cleanup.callback(self.server.stop)
        self.server.wait_ready(90)
        settings = request(f"{self.server.url}/api/config/settings")
        project = settings["projects"][0]
        self.project_id = project["id"]
        self.project_path = project["path"].rstrip("/")
        encoded = __import__("base64").urlsafe_b64encode(self.project_path.encode()).decode().rstrip("=")
        self.context_id = f"path_{encoded}"
        state = args.output / "native-state"
        for directory in runner.state_dirs(state).values():
            Path(directory).mkdir(parents=True, exist_ok=True)
        self.env = runner.native_env(state, self.server.url, "openchamber-dark")
        log = cleanup.enter_context((args.output / "native.log").open("w"))
        self.client = NativeClient(args.binary, self.env, args.scale, 1440, 900, stderr=log)
        cleanup.callback(self.close)
        self.client.launch()
        self.client.wait_ready(90, connected=True)

    def close(self):
        process = self.client.process
        if process is None:
            return {}
        exited = process.poll()
        if exited is not None:
            return {"exitCode": exited}
        return self.client.close()

    def tasks(self) -> list:
        quoted = urllib.parse.quote(self.project_id, safe="")
        return request(f"{self.server.url}/api/projects/{quoted}/scheduled-tasks")["tasks"]

    def context(self) -> dict:
        return request(f"{self.server.url}/api/project-context/{self.context_id}")

    def ids(self) -> set[str]:
        return {node.get_accessible_id() or "" for node in self.client._walk()}

    def has(self, accessible_id: str) -> bool:
        return accessible_id in self.ids()

    def label(self, accessible_id: str) -> str:
        try:
            return self.client._find(accessible_id).get_name() or ""
        except LookupError:
            return ""

    def wait_id(self, accessible_id: str, timeout=30):
        wait_for(f"{accessible_id} did not appear", lambda: self.has(accessible_id), timeout=timeout)

    def wait_gone(self, accessible_id: str, timeout=30):
        wait_for(f"{accessible_id} did not go away", lambda: not self.has(accessible_id), timeout=timeout)

    def type(self, text: str):
        self.client._activate()
        command(self.env, "xdotool", "type", "--delay", "25", "--", text)

    def key(self, name: str):
        self.client._activate()
        command(self.env, "xdotool", "key", name)

    def click_node(self, accessible_id: str):
        self.client._activate()
        x, y = self.client._screen_point(self.client._find(accessible_id))
        command(self.env, "xdotool", "mousemove", str(x), str(y), "click", "1")

    def scroll_down(self, accessible_id: str, steps: int = 10):
        """Turns the wheel over an element, scrolling what holds it."""
        self.client._activate()
        x, y = self.client._screen_point(self.client._find(accessible_id))
        command(self.env, "xdotool", "mousemove", str(x), str(y))
        for _ in range(steps):
            command(self.env, "xdotool", "click", "5")
        time.sleep(0.3)

    def screenshot(self, name: str):
        self.client.screenshot(self.args.output / f"{name}.png")


def steps(session: Session):
    client = session.client
    task = {}

    def schedules_empty():
        client.click("scheduled-tasks")
        session.wait_id("scheduled-tasks-page")
        client.click(f"scheduled-tasks-scope:{session.project_id}")
        session.wait_id("scheduled-tasks-empty")

    def create_in_zone():
        def name_ready(expected=None):
            node = client._find("scheduled-task-name")
            node.clear_cache()
            return (node.get_state_set().contains(_atspi().StateType.FOCUSED)
                    and (expected is None or editable_text(node) == expected))

        client.click("scheduled-tasks-new")
        session.wait_id("scheduled-task-editor")
        session.wait_id("scheduled-task-name")
        session.click_node("scheduled-task-name")
        wait_for("the clicked task-name input never gained focus", name_ready, timeout=10)
        session.type(TASK_NAME)
        wait_for("the focused task-name input never contained the dispatched text",
                 lambda: name_ready(TASK_NAME), timeout=10)
        client.click("scheduled-task-use-defaults")
        # The prompt sits below the dialog's fold.
        session.scroll_down("scheduled-task-name")
        session.click_node("scheduled-task-prompt-frame")
        session.type("Summarize what changed today.")
        client.click("scheduled-task-timezone")
        session.wait_id("scheduled-task-timezone-filter")
        session.type("Kyiv")
        session.wait_id(f"scheduled-task-timezone:{ZONE}")
        client.click(f"scheduled-task-timezone:{ZONE}")
        client.click("scheduled-task-save")
        session.wait_gone("scheduled-task-editor")
        stored = session.tasks()
        if len(stored) != 1:
            raise RuntimeError(f"expected one stored task: {stored}")
        schedule = stored[0]["schedule"]
        if schedule.get("kind") != "daily" or schedule.get("times") != ["09:00"] or schedule.get("timezone") != ZONE:
            raise RuntimeError(f"the stored schedule is wrong: {schedule}")
        if stored[0]["name"] != TASK_NAME or stored[0]["execution"].get("useDefaults") is not True:
            raise RuntimeError(f"the stored task is wrong: {stored[0]}")
        task["id"] = stored[0]["id"]
        session.wait_id(f"scheduled-task:{task['id']}")

    def invalid():
        client.click("scheduled-tasks-new")
        session.wait_id("scheduled-task-editor")
        client.click("scheduled-task-kind")
        session.wait_id("scheduled-task-kind:cron")
        client.click("scheduled-task-kind:cron")
        session.wait_id("scheduled-task-next-runs")
        session.click_node("scheduled-task-cron")
        session.key("ctrl+a")
        session.type("61 * * * *")
        session.wait_id("scheduled-task-cron-invalid")
        session.wait_gone("scheduled-task-next-runs")
        client.click("scheduled-task-save")
        session.wait_id("scheduled-task-editor-error")
        if len(session.tasks()) != 1:
            raise RuntimeError("an invalid task reached the server")
        client.click("scheduled-task-cancel")
        session.wait_gone("scheduled-task-editor")

    def disable_enable():
        toggle = f"scheduled-task-toggle:{task['id']}"
        client.click(toggle)
        wait_for("the task never paused", lambda: session.tasks()[0]["enabled"] is False, timeout=20)
        wait_for("the row never read paused", lambda: "Paused" in session.label(toggle), timeout=20)
        client.click(toggle)
        wait_for("the task never resumed", lambda: session.tasks()[0]["enabled"] is True, timeout=20)

    def run_now():
        client.click(f"scheduled-task-run:{task['id']}")
        session.wait_gone("scheduled-tasks-page", timeout=60)
        wait_for("the run recorded no session",
                 lambda: bool(session.tasks()[0].get("state", {}).get("lastSessionId")), timeout=60)
        client.click("scheduled-tasks")
        session.wait_id(f"scheduled-task-open:{task['id']}")

    def delete():
        client.click(f"scheduled-task-delete:{task['id']}")
        session.wait_id("scheduled-task-delete-confirm")
        client.click("scheduled-task-delete-confirm")
        session.wait_id("scheduled-tasks-empty")
        if session.tasks():
            raise RuntimeError("the task was not deleted")
        client.click("scheduled-tasks")
        session.wait_gone("scheduled-tasks-page")

    def notes():
        client.click("rail-notes")
        session.wait_id("knowledge-panel")
        session.wait_id("knowledge-note-input")
        session.click_node("knowledge-note-input")
        session.type("Release checklist")
        session.key("Return")
        wait_for("the note was not stored",
                 lambda: [note["body"] for note in session.context()["notes"]] == ["Release checklist"], timeout=20)
        note = session.context()["notes"][0]["id"]
        session.wait_id(f"knowledge-note:{note}")
        client.click(f"knowledge-note:{note}")
        session.wait_id("knowledge-note-editor")
        session.click_node("knowledge-note-editor")
        session.key("ctrl+End")
        session.type(" for 2.7")
        wait_for("the edit was not saved",
                 lambda: session.context()["notes"][0]["body"] == "Release checklist for 2.7", timeout=20)
        client.click("knowledge-back")
        session.wait_id(f"knowledge-note:{note}")

    def todos():
        client.click("knowledge-tab:todos")
        session.wait_id("knowledge-todo-input")
        session.click_node("knowledge-todo-input")
        session.type("Write the release notes")
        session.key("Return")
        wait_for("the todo was not stored", lambda: len(session.context()["todos"]) == 1, timeout=20)
        todo = session.context()["todos"][0]["id"]
        client.click(f"knowledge-todo-check:{todo}")
        wait_for("the todo was not completed", lambda: session.context()["todos"][0]["completed"] is True, timeout=20)
        session.wait_id("knowledge-todo-clear")

    def plan_dispatch():
        client.click("knowledge-tab:plans")
        session.wait_id("knowledge-plan-input")
        session.click_node("knowledge-plan-input")
        session.type("Launch")
        session.key("Return")
        session.wait_id("knowledge-plan-editor", timeout=30)
        plan = session.context()["plans"][0]["id"]
        session.click_node("knowledge-plan-editor")
        session.key("ctrl+End")
        session.key("Return")
        for line in ("## Steps", "- [ ] Ship it"):
            session.type(line)
            session.key("Return")
        wait_for("the plan text was not saved", lambda: (session.context()["plans"][0].get("progress") or {}).get("total") == 1,
                 timeout=20)
        client.click("knowledge-plan-implement")
        wait_for("the plan was not linked to a session",
                 lambda: bool((session.context()["plans"][0].get("session") or {}).get("sessionId")), timeout=60)
        client.click("knowledge-back")
        session.wait_id(f"knowledge-plan-session:{plan}", timeout=30)

    def search():
        client.click("knowledge-search-toggle")
        session.wait_id("knowledge-search")
        session.type("release")
        wait_for("the search did not follow to notes",
                 lambda: "Notes" in session.label("knowledge-tab:notes") and session.has(
                     f"knowledge-note:{session.context()['notes'][0]['id']}"), timeout=20)
        client.click("knowledge-search-close")
        session.wait_gone("knowledge-search")

    return [
        ("schedules-empty", schedules_empty),
        ("schedule-created-in-zone", create_in_zone),
        ("schedule-invalid", invalid),
        ("schedule-disabled-enabled", disable_enable),
        ("schedule-run-now", run_now),
        ("schedule-deleted", delete),
        ("knowledge-notes", notes),
        ("knowledge-todos", todos),
        ("knowledge-plan-dispatched", plan_dispatch),
        ("knowledge-search", search),
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
            for name, step in steps(session):
                try:
                    step()
                    results.append({"check": name, "passed": True})
                    session.screenshot(name)
                except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, AssertionError, KeyError,
                        IndexError, subprocess.SubprocessError) as error:
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
            exit_code = session.client.process.wait(timeout=120)
            elapsed = time.monotonic() - started
            results.append({"check": "close", "passed": exit_code == 0 and elapsed < 15,
                            "seconds": round(elapsed, 2), "exitCode": exit_code})
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
    args.output = (args.output or Path("/tmp/opencode")
                   / f"native-knowledge-schedules-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
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
