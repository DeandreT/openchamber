"""Qualify native fork, staged revert, /btw and subagent navigation against an
owned real engine.

Forks a seeded session from a prompt, stages and restores a revert, commits
one by sending the next prompt and checks it after a relaunch, fails one stage
on purpose through a loopback proxy, opens and destroys a /btw side
conversation, and opens a subagent's child from its report and returns. Every
outcome is confirmed through the isolated server's API.
"""

import argparse
from contextlib import ExitStack
import http.client
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request

from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, editable_text, wait_for
from native_session_management import ActionFaults
import runner
import virtual_display
from workflow_proxy import FaultProxy

COMMITTED = "Native revert commit ok"
BTW_QUESTION = "/btw what does this session cover"


class StageFaults(ActionFaults):
    """Fails only the armed requests. `ActionFaults` also loses the first
    prompt's answer; this check sends prompts that must arrive, and the
    native client creates no session through the proxy."""

    def __init__(self, upstream):
        FaultProxy.__init__(self, upstream, "after", "create")
        self.rules = []
        self.failed = []
        self.held_path = None
        self.held = threading.Event()
        self.release = threading.Event()

    def hold_next(self, path):
        with self.lock:
            self.held_path = path
            self.held.clear()
            self.release.clear()

    def handle(self, handler):
        with self.lock:
            hold = self.held_path == urllib.parse.urlsplit(handler.path).path
            if hold:
                self.held_path = None
        if hold:
            self.held.set()
            self.release.wait(30)
        super().handle(handler)

    def stop(self):
        self.release.set()
        super().stop()


def quote(session):
    return urllib.parse.quote(session, safe="")


def user_prompts(messages):
    """User messages in server order, as (id, text)."""
    ordered = sorted(messages, key=lambda message: (message.get("time") or {}).get("created", 0))
    return [(message["id"], message.get("text", "")) for message in ordered if message.get("type") == "user"]


def message_id(now_ms=None):
    """A client message ID in the pinned OpenCode layout (`message_id.rs`)."""
    now_ms = int(time.time() * 1000) if now_ms is None else now_ms
    alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    suffix = "".join(alphabet[byte % len(alphabet)] for byte in os.urandom(14))
    return f"msg_{(now_ms * 4096 + 1) & 0xFFFFFFFFFFFF:012x}{suffix}"


def revert_marker(record):
    return ((record or {}).get("revert") or {}).get("messageID")


def btw_link(record):
    return (((record or {}).get("metadata") or {}).get("openchamber") or {}).get("btwSessionID")


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()
    clients = []

    def check(name, run):
        try:
            run()
            results.append({"check": name, "passed": True})
        except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration, KeyError,
                subprocess.SubprocessError, http.client.HTTPException) as error:
            traceback.print_exc()
            results.append({"check": name, "passed": False, "error": str(error)})
            try:
                clients[-1].screenshot(args.output / f"{name}-failed.png")
                (args.output / f"{name}-ids.txt").write_text("\n".join(sorted(
                    node.get_accessible_id() or "" for node in clients[-1]._walk())))
            except Exception:  # noqa: BLE001 - evidence is best effort after a failure
                pass
            raise

    try:
        with ExitStack() as cleanup:
            state = runner.copy_seed(args.root, args.output, "openchamber-dark")
            server = runner.IsolatedServer(args.output, args)
            cleanup.callback(server.stop)
            server.wait_ready(60)
            proxy = StageFaults(server.url)
            cleanup.callback(proxy.stop)
            env = runner.native_env(state, proxy.url, "openchamber-dark")
            set_accessibility(True)
            log = cleanup.enter_context((args.output / "native.log").open("w"))

            def launch():
                client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                cleanup.callback(client.close)
                client.launch()
                client.wait_ready(60, connected=True)
                client.wait_for_marker("composer-usable", 30)
                clients.append(client)
                return client

            client = launch()

            def request(path, method="GET", body=None):
                data = None if body is None else json.dumps(body).encode()
                headers = {"Content-Type": "application/json"} if body is not None else {}
                req = urllib.request.Request(f"{server.url}{path}", data=data, method=method, headers=headers)
                with urllib.request.urlopen(req, timeout=15) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None

            def record(session):
                try:
                    return request(f"/api/session/{quote(session)}")["data"]
                except urllib.error.HTTPError as error:
                    if error.code == 404:
                        return None
                    raise

            def messages(session):
                return request(f"/api/session/{quote(session)}/message?limit=200")["data"]

            def sessions():
                return request("/api/session?limit=200")["data"]

            def ids():
                return {node.get_accessible_id() or "" for node in clients[-1]._walk()}

            def notice():
                try:
                    return clients[-1]._find("session-notice").get_name() or ""
                except LookupError:
                    return ""

            def entries():
                return [node for node in clients[-1]._walk() if node.get_role_name() == "entry"]

            def composer():
                nodes = entries()
                return editable_text(nodes[0]) if len(nodes) == 1 else None

            def focus_editor():
                clients[-1]._activate()
                point = []

                def locate():
                    nodes = entries()
                    if len(nodes) != 1:
                        return False
                    point[:] = clients[-1]._screen_point(nodes[0])
                    return True

                wait_for("the composer did not appear", locate)
                command(env, "xdotool", "mousemove", str(point[0]), str(point[1]), "click", "1")

            def type_into_composer(text, replace=False):
                focus_editor()
                if replace:
                    command(env, "xdotool", "key", "ctrl+a", "BackSpace")
                    wait_for("the composer did not clear", lambda: composer() == "")
                command(env, "xdotool", "type", "--delay", "20", text)
                wait_for("the typed text did not reach the composer", lambda: composer() == text)

            def reveal(message_id):
                """Scrolls the transcript up until the message's row is rendered;
                the list renders only rows near the viewport."""
                if f"message:{message_id}" in ids():
                    return
                x, y = clients[-1]._screen_point(clients[-1]._find("transcript"))
                command(env, "xdotool", "mousemove", str(x), str(y))
                for _ in range(120):
                    command(env, "xdotool", "click", "4")
                    if f"message:{message_id}" in ids():
                        time.sleep(0.2)
                        return
                raise LookupError(f"{message_id} never scrolled into view")

            def row_key(session):
                keys = [row["key"] for row in clients[-1].rows() if row["key"].endswith(f":session:{session}")]
                return keys[0] if keys else None

            def open_session(session):
                key = row_key(session)
                if key is None:
                    raise LookupError(f"{session} has no sidebar row")
                clients[-1].click(key)
                if not runner.wait_selected(clients[-1], key, 15):
                    raise RuntimeError(f"{session} did not open")

            def seeded(title):
                return next(entry["id"] for entry in sessions() if entry.get("title") == title)

            short_a, short_b, long = seeded("parity: short a"), seeded("parity: short b"), seeded("parity: long")

            def fork_from_message():
                prompts = user_prompts(messages(short_b))
                if len(prompts) < 3:
                    raise RuntimeError("the seed needs three prompts in short b")
                target, text = prompts[1]
                before = {entry["id"] for entry in sessions()}
                open_session(short_b)
                reveal(target)
                clients[-1].activate(f"message-fork:{target}")
                created = []

                def forked():
                    created[:] = [entry for entry in sessions() if entry["id"] not in before]
                    return bool(created)

                wait_for("the server has no fork", forked, timeout=20)
                fork = created[0]["id"]
                kept = user_prompts(messages(fork))
                if [prompt_text for _, prompt_text in kept] != [prompts[0][1]]:
                    raise RuntimeError(f"the fork kept {len(kept)} prompts, not only the first")
                wait_for("the fork did not open", lambda: any(
                    row["key"].endswith(f":session:{fork}") and row.get("selected") for row in clients[-1].rows()), timeout=15)
                wait_for("the forked prompt did not reach the fork's composer", lambda: composer() == text.strip(), timeout=10)
                clients[-1].screenshot(args.output / "forked.png")

            def stage_and_restore():
                prompts = user_prompts(messages(short_a))
                target, text = prompts[-1]
                open_session(short_a)
                reveal(target)
                clients[-1].activate(f"message-revert:{target}")
                wait_for("the server did not stage the revert", lambda: revert_marker(record(short_a)) is not None, timeout=20)
                wait_for("the strip did not appear", lambda: "revert-strip" in ids())
                wait_for("the reverted prompt row stayed", lambda: f"message:{target}" not in ids())
                wait_for("the prompt did not return to the composer", lambda: composer() == text.strip())
                clients[-1].screenshot(args.output / "revert-staged.png")
                clients[-1].activate("revert-restore")
                wait_for("the server kept the staged revert", lambda: revert_marker(record(short_a)) is None, timeout=20)
                wait_for("the strip stayed", lambda: "revert-strip" not in ids())
                wait_for("the restored prompt did not come back", lambda: (reveal(target), True)[1], timeout=20)
                clients[-1].screenshot(args.output / "revert-restored.png")

            def failed_stage_rolls_back():
                prompts = user_prompts(messages(short_a))
                target, _ = prompts[-1]
                type_into_composer("draft before the failed revert", replace=True)
                reveal(target)
                proxy.fail_next("POST", rf"/api/session/{re.escape(short_a)}/revert/stage")
                clients[-1].activate(f"message-revert:{target}")
                wait_for("the stage never reached the proxy", lambda: bool(proxy.failed), timeout=20)
                wait_for("the failure was not announced", lambda: notice().startswith("Revert from here"), timeout=10)
                wait_for("the optimistic marker stayed", lambda: "revert-strip" not in ids())
                wait_for("the prompt row did not come back", lambda: (reveal(target), True)[1], timeout=20)
                wait_for("the composer did not return to the draft",
                         lambda: composer() == "draft before the failed revert", timeout=10)
                if revert_marker(record(short_a)) is not None:
                    raise RuntimeError("the server staged a revert the proxy refused")
                clients[-1].screenshot(args.output / "revert-failed.png")

            def commit_survives_relaunch():
                nonlocal client
                prompts = user_prompts(messages(short_a))
                target, _ = prompts[-1]
                reveal(target)
                clients[-1].activate(f"message-revert:{target}")
                wait_for("the server did not stage the revert", lambda: revert_marker(record(short_a)) is not None, timeout=20)
                type_into_composer(COMMITTED, replace=True)
                clients[-1].key("Return")
                wait_for("the next prompt did not commit the revert", lambda: (
                    revert_marker(record(short_a)) is None
                    and target not in [message_id for message_id, _ in user_prompts(messages(short_a))]
                    and COMMITTED in [prompt for _, prompt in user_prompts(messages(short_a))]), timeout=60)
                wait_for("the committed prompt stayed in the transcript", lambda: f"message:{target}" not in ids(), timeout=20)
                clients[-1].screenshot(args.output / "revert-committed.png")
                clients[-1].close()
                client = launch()
                open_session(short_a)
                committed = next(message_id for message_id, prompt in user_prompts(messages(short_a)) if prompt == COMMITTED)
                wait_for("the new prompt did not load after the relaunch", lambda: (reveal(committed), True)[1], timeout=30)
                if f"message:{target}" in ids() or "revert-strip" in ids():
                    raise RuntimeError("the relaunched client shows the reverted prompt")
                clients[-1].screenshot(args.output / "revert-after-relaunch.png")

            def btw_pending_survives_relaunch():
                nonlocal client
                open_session(long)
                type_into_composer(BTW_QUESTION, replace=True)
                path = f"/api/session/{long}/fork"
                proxy.fail_next("POST", re.escape(path))
                proxy.hold_next(path)
                clients[-1].key("Return")
                wait_for("the BTW start did not reach the proxy", proxy.held.is_set)
                if composer() != BTW_QUESTION:
                    raise RuntimeError("the pending BTW start cleared the question")
                clients[-1].screenshot(args.output / "btw-pending-draft.png")
                clients[-1].click("window-close")
                clients[-1].process.wait(timeout=15)
                proxy.release.set()
                client = launch()
                open_session(long)
                wait_for("the pending BTW draft was lost on relaunch", lambda: composer() == BTW_QUESTION)
                type_into_composer("", replace=True)

            def btw_create_and_destroy():
                open_session(long)
                type_into_composer(BTW_QUESTION)
                clients[-1].key("Return")
                wait_for("the parent was not linked", lambda: btw_link(record(long)) is not None, timeout=30)
                fork = btw_link(record(long))
                marker = ((record(fork) or {}).get("metadata") or {}).get("openchamber") or {}
                if marker.get("kind") != "btw" or marker.get("originalSessionID") != long:
                    raise RuntimeError("the side conversation is not marked as the parent's /btw fork")
                wait_for("the panel did not open", lambda: "btw-panel" in ids(), timeout=20)
                wait_for("the question did not show in the panel",
                         lambda: any(item.startswith("btw-message:") for item in ids()), timeout=30)
                if row_key(fork) is not None:
                    raise RuntimeError("the side conversation has a sidebar row")
                clients[-1].screenshot(args.output / "btw-open.png")
                clients[-1].activate("btw-destroy")
                wait_for("the fork was not deleted", lambda: record(fork) is None, timeout=20)
                wait_for("the parent kept its link", lambda: btw_link(record(long)) is None, timeout=20)
                wait_for("the panel stayed", lambda: "btw-panel" not in ids())
                clients[-1].screenshot(args.output / "btw-destroyed.png")

            def subagent_open_and_return():
                directory = record(short_b)["location"]["directory"]
                child = request("/api/session", "POST", {"parentID": short_b, "location": {"directory": directory},
                                                        "title": "parity: subagent child"})["data"]["id"]
                if (record(child) or {}).get("parentID") != short_b:
                    raise RuntimeError("the engine did not keep the child's parent")
                # Resumed, so a short fixture turn delivers the report into history.
                report = message_id()
                request(f"/api/session/{quote(short_b)}/synthetic", "POST", {
                    "id": report, "text": "<subagent>done</subagent>", "description": "Parity subagent report",
                    "resume": True,
                    "metadata": {"source": "subagent", "childID": child, "agent": "explore", "state": "completed"},
                })
                wait_for("the report did not reach history",
                         lambda: any(message["id"] == report for message in messages(short_b)), timeout=60)
                open_session(short_b)
                wait_for("the report did not load", lambda: (reveal(report), True)[1], timeout=30)
                wait_for("the report has no Open control", lambda: f"subtask-open:{report}" in ids(), timeout=10)
                clients[-1].screenshot(args.output / "subagent-report.png")
                clients[-1].activate(f"subtask-open:{report}")
                wait_for("the child did not open", lambda: "return-to-parent" in ids(), timeout=20)
                clients[-1].screenshot(args.output / "subagent-child.png")
                clients[-1].activate("return-to-parent")
                wait_for("the parent did not open again", lambda: (reveal(report), True)[1], timeout=20)
                if "return-to-parent" in ids():
                    raise RuntimeError("the parent shows Return to parent")

            steps = [("fork-from-message", fork_from_message), ("stage-and-restore", stage_and_restore),
                     ("failed-stage-rolls-back", failed_stage_rolls_back),
                     ("commit-survives-relaunch", commit_survives_relaunch),
                     ("btw-pending-survives-relaunch", btw_pending_survives_relaunch),
                     ("btw-create-destroy", btw_create_and_destroy),
                     ("subagent-open-return", subagent_open_and_return)]
            for name, run in steps:
                if args.only and name not in args.only:
                    continue
                check(name, run)
            clients[-1].click("window-close")
            clients[-1].process.wait(timeout=15)
    except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration, KeyError,
            subprocess.SubprocessError) as error:
        if not results or results[-1]["passed"]:
            traceback.print_exc()
            results.append({"check": "fork-revert", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({
            "binary": binary_info(args.binary), "display": "owned-xvfb", "scale": args.scale, "checks": results,
        }, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--root", type=Path, default=runner.REPO_ROOT / "tmp/parity")
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--only", action="append", help="run only the named check; repeatable")
    args = parser.parse_args()
    args.root = args.root.resolve()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"native-fork-revert-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists() or not (args.root / "seed").is_dir():
        parser.error("choose a new output directory and an existing parity seed")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
