"""Qualify the native composer's queue, steer, starters and double-Escape stop
against an owned real engine.

Runs on the parity seed's isolated server with the fixture's think-30s model,
which keeps a turn running long enough to queue, edit, reorder, remove, steer
and restart the window in it. Every outcome is confirmed through the server's
own queue and message routes, not only through what the window shows.

Run: python3 scripts/parity/native_queue.py --output <new directory>
"""

import argparse
from contextlib import ExitStack
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request

from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, editable_text, wait_for
import runner
import virtual_display

BUSY_MODEL = {"providerID": "perf", "id": "think-30s"}
STARTER_DRAFT = "native starter idea"


def queued_contents(snapshot, session):
    """The queued messages' typed text for one session, in queue order."""
    for entry in snapshot.get("sessions", []):
        if entry.get("sessionId") == session:
            return [item.get("content") for item in entry.get("items", [])]
    return []


def queued_ids(snapshot, session):
    for entry in snapshot.get("sessions", []):
        if entry.get("sessionId") == session:
            return [item.get("id") for item in entry.get("items", [])]
    return []


def user_texts(messages):
    """The text of every user message, oldest first, as history reports it."""
    return [message.get("text") for message in messages if message.get("type") == "user"]


def starter_names(settings):
    """The saved global starter names, or None while the defaults apply."""
    starters = settings.get("draftStarters")
    if not isinstance(starters, list):
        return None
    return [entry.get("name") for entry in starters if isinstance(entry, dict)]


def verify_starter_prompt(text, draft):
    """A Craft a Goal chip renders its visible prompt with the draft as the idea."""
    if not isinstance(text, str) or not text.endswith(f"Here is my initial idea:\n{draft}"):
        raise ValueError("the starter did not render its prompt with the draft as its idea")
    if text.startswith("/"):
        raise ValueError("the starter sent its command instead of the rendered prompt")


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()
    clients = []

    def check(name, run):
        started = time.monotonic()
        try:
            detail = run()
            results.append({"check": name, "passed": True, "seconds": round(time.monotonic() - started, 1),
                            **({"detail": detail} if detail else {})})
        except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration,
                subprocess.SubprocessError, http.client.HTTPException) as error:
            traceback.print_exc()
            results.append({"check": name, "passed": False, "error": str(error)})
            try:
                clients[-1].screenshot(args.output / f"{name}-failed.png")
            except Exception:  # noqa: BLE001 - evidence is best effort after a failure
                pass
            raise

    try:
        with ExitStack() as cleanup:
            state = runner.copy_seed(args.root, args.output, "openchamber-dark")
            server = runner.IsolatedServer(args.output, args)
            cleanup.callback(server.stop)
            server.wait_ready(60)
            env = runner.native_env(state, server.url, "openchamber-dark")
            set_accessibility(True)

            def request(path, method="GET", body=None):
                data = json.dumps(body).encode() if body is not None else None
                headers = {"Content-Type": "application/json"} if data is not None else {}
                call = urllib.request.Request(f"{server.url}{path}", data=data, method=method, headers=headers)
                with urllib.request.urlopen(call, timeout=15) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None

            def launch(label):
                log = cleanup.enter_context((args.output / f"native-{label}.log").open("w"))
                client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                cleanup.callback(lambda: client.process is not None and client.process.poll() is None and client.close())
                client.launch()
                client.wait_ready(60, connected=True)
                client.wait_for_marker("composer-usable", 30)
                clients.append(client)
                return client

            client = launch("first")

            def ids():
                return {node.get_accessible_id() or "" for node in clients[-1]._walk()}

            def entries():
                return [node for node in clients[-1]._walk() if node.get_role_name() == "entry"]

            def composer_text():
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
                time.sleep(0.2)

            def type_text(text):
                focus_editor()
                command(env, "xdotool", "type", "--delay", "20", text)
                wait_for("the text did not reach the composer", lambda: composer_text() == text)

            roots = [entry for entry in request("/api/session?limit=200")["data"]
                     if not entry.get("parentID") and not (entry.get("time") or {}).get("archived")]

            def row_key(session):
                keys = [row["key"] for row in clients[-1].rows() if row["key"].endswith(f":session:{session}")]
                return keys[0] if keys else None

            listed = [entry["id"] for entry in roots if row_key(entry["id"])]
            if not listed:
                raise RuntimeError("the seed lists no root session")
            session = listed[0]
            quoted = urllib.parse.quote(session, safe="")
            request(f"/api/session/{quoted}/model", "POST", {"model": BUSY_MODEL})
            wait_for("the busy model was not selected",
                     lambda: ((request(f"/api/session/{quoted}")["data"].get("model") or {}).get("id")) == "think-30s")

            def select():
                key = row_key(session)
                clients[-1].click(key)
                if not runner.wait_selected(clients[-1], key):
                    raise RuntimeError("the session did not open")

            def snapshot():
                return request("/api/message-queue")

            def queued():
                return queued_contents(snapshot(), session)

            def texts():
                return user_texts(request(f"/api/session/{quoted}/message?limit=200")["data"])

            def active():
                return session in (request("/api/session/active").get("data") or {})

            def start_turn(text):
                wait_for("the previous turn did not end", lambda: not active(), timeout=90)
                type_text(text)
                clients[-1].key("Return")
                wait_for("the turn did not start", active, timeout=30)
                wait_for("the prompt was not admitted", lambda: text in texts(), timeout=30)

            def enter(text):
                type_text(text)
                clients[-1].key("Return")

            def ids_of(label):
                order = queued_ids(snapshot(), session)
                return dict(zip(queued(), order)).get(label)

            def drag(source, target):
                clients[-1]._activate()
                start = clients[-1]._screen_point(clients[-1]._find(source))
                end = clients[-1]._screen_point(clients[-1]._find(target))
                command(env, "xdotool", "mousemove", str(start[0]), str(start[1]), "mousedown", "1")
                for step in range(1, 11):
                    x = start[0] + (end[0] - start[0]) * step // 10
                    y = start[1] + (end[1] - start[1]) * step // 10
                    command(env, "xdotool", "mousemove", str(x), str(y))
                    time.sleep(0.03)
                command(env, "xdotool", "mouseup", "1")

            select()

            def queue():
                start_turn("Queue base turn")
                enter("Queued alpha")
                wait_for("the server did not queue alpha", lambda: queued() == ["Queued alpha"], timeout=20)
                if composer_text() != "":
                    raise RuntimeError("queueing left the text in the composer")
                wait_for("the strip did not show", lambda: "queued-strip" in ids())
                enter("Queued beta")
                wait_for("the server did not queue beta", lambda: queued() == ["Queued alpha", "Queued beta"], timeout=20)
                if "Queued alpha" in texts():
                    raise RuntimeError("a queued message was sent while the turn ran")
                clients[-1].click("queued-toggle")
                wait_for("the list did not open", lambda: f"queued-row-{ids_of('Queued beta')}" in ids())
                clients[-1].screenshot(args.output / "queue-two.png")
                return {"queued": queued()}

            def reorder_remove():
                alpha, beta = ids_of("Queued alpha"), ids_of("Queued beta")
                drag(f"queued-handle-{beta}", f"queued-row-{alpha}")
                wait_for("the server did not reorder", lambda: queued() == ["Queued beta", "Queued alpha"], timeout=20)
                clients[-1].click(f"queued-remove-{alpha}")
                wait_for("the server did not remove alpha", lambda: queued() == ["Queued beta"], timeout=20)
                clients[-1].screenshot(args.output / "queue-reordered-removed.png")
                return {"queued": queued()}

            def queue_edit():
                beta = ids_of("Queued beta")
                clients[-1].click(f"queued-edit-{beta}")
                wait_for("edit did not take the message", lambda: queued() == [], timeout=20)
                wait_for("edit did not fill the composer", lambda: composer_text() == "Queued beta")
                command(env, "xdotool", "key", "Return")
                wait_for("the edited message was not queued again", lambda: queued() == ["Queued beta"], timeout=20)
                wait_for("the composer did not clear", lambda: composer_text() == "")
                # ArrowUp on an empty composer edits the newest queued message.
                focus_editor()
                command(env, "xdotool", "key", "Up")
                wait_for("ArrowUp did not take the message", lambda: queued() == [], timeout=20)
                wait_for("ArrowUp did not fill the composer", lambda: composer_text() == "Queued beta")
                command(env, "xdotool", "key", "Return")
                wait_for("the message was not queued again", lambda: queued() == ["Queued beta"], timeout=20)
                return {"queued": queued()}

            def steer():
                if not active():
                    raise RuntimeError("the base turn ended before the steer")
                beta = ids_of("Queued beta")
                clients[-1].click(f"queued-send-{beta}")
                wait_for("the steer was not admitted", lambda: "Queued beta" in texts(), timeout=30)
                if queued():
                    raise RuntimeError("the sent message stayed queued")
                return {"activeAtAdmission": active()}

            def queue_restart():
                start_turn("Restart base turn")
                enter("Queued across restart")
                wait_for("the server did not queue it", lambda: queued() == ["Queued across restart"], timeout=20)
                clients[-1].click("window-close")
                clients[-1].process.wait(timeout=15)
                launch("restart")
                select()
                wait_for("the strip did not return after restart", lambda: "queued-strip" in ids(), timeout=20)
                clients[-1].screenshot(args.output / "queue-after-restart.png")
                # Delivery: the server sends it when the turn ends.
                wait_for("the server did not deliver the queue", lambda: "Queued across restart" in texts(), timeout=120)
                wait_for("the delivered message stayed queued", lambda: queued() == [], timeout=20)
                wait_for("the strip did not clear", lambda: "queued-strip" not in ids(), timeout=20)
                return {"delivered": True}

            def double_escape():
                wait_for("the delivered turn did not run", active, timeout=30)
                focus_editor()
                command(env, "xdotool", "key", "Escape")
                time.sleep(1.5)
                if not active():
                    raise RuntimeError("one Escape stopped the turn")
                time.sleep(2.0)
                command(env, "xdotool", "key", "Escape")
                time.sleep(1.0)
                if not active():
                    raise RuntimeError("an Escape after three seconds stopped the turn")
                command(env, "xdotool", "key", "Escape")
                wait_for("the second Escape did not stop the turn", lambda: not active(), timeout=20)
                return {"stopped": True}

            def starter():
                clients[-1].click("new-session")
                wait_for("the new-session view did not open", lambda: "startup" in ids() or "starter-add" in ids(), timeout=20)
                chip = "starter-global-command-craft-goal"
                wait_for("the starter chip did not show", lambda: chip in ids(), timeout=20)
                before = {entry["id"] for entry in request("/api/session?limit=200")["data"]}
                # A draft creates a session only in a chosen project.
                clients[-1].click("choose-project")
                option = []

                def listed():
                    option[:] = [item for item in ids() if item.startswith("project-option:")]
                    return bool(option)

                wait_for("no project was offered", listed, timeout=20)
                clients[-1].click(option[0])
                wait_for("the project was not chosen", lambda: not any(item.startswith("project-option:") for item in ids()))
                type_text(STARTER_DRAFT)
                clients[-1].click(chip)

                fresh = []

                def created():
                    fresh[:] = [entry["id"] for entry in request("/api/session?limit=200")["data"] if entry["id"] not in before]
                    return bool(fresh)

                wait_for("the starter did not create a session", created, timeout=30)
                new = fresh[0]
                new_quoted = urllib.parse.quote(new, safe="")
                wait_for("the starter prompt was not admitted", lambda: user_texts(
                    request(f"/api/session/{new_quoted}/message?limit=50")["data"]), timeout=30)
                first = user_texts(request(f"/api/session/{new_quoted}/message?limit=50")["data"])[0]
                verify_starter_prompt(first, STARTER_DRAFT)
                clients[-1].screenshot(args.output / "starter-sent.png")
                return {"session": new}

            def starter_management():
                clients[-1].click("new-session")
                chip = "starter-global-command-explore"
                wait_for("the explore chip did not show", lambda: chip in ids(), timeout=20)
                clients[-1].activate(f"{chip}-remove")
                wait_for("the removal was not saved", lambda: "explore" not in (
                    starter_names(request("/api/config/settings?surface=desktop")) or ["explore"]), timeout=20)
                wait_for("the chip did not leave", lambda: chip not in ids())
                saved = request("/api/config/settings?surface=desktop")
                if saved.get("draftStartersCraftGoalAdded") is not True or saved.get("draftStartersScheduleTaskAdded") is not True:
                    raise RuntimeError("the markers that keep a removal were not saved")
                clients[-1].click("starter-add")
                wait_for("the picker did not open", lambda: "starter-pick-explore" in ids(), timeout=10)
                clients[-1].click("starter-pick-explore")
                wait_for("the pin was not saved", lambda: "explore" in (starter_names(request("/api/config/settings?surface=desktop")) or []), timeout=20)
                wait_for("the chip did not return", lambda: chip in ids())
                clients[-1].click("window-close")
                clients[-1].process.wait(timeout=15)
                launch("starters-restart")
                clients[-1].click("new-session")
                wait_for("the pinned chip did not survive a restart", lambda: chip in ids(), timeout=20)
                return {"saved": starter_names(request("/api/config/settings?surface=desktop"))}

            steps = [("queue", queue), ("reorder-remove", reorder_remove), ("queue-edit", queue_edit),
                     ("steer", steer), ("queue-restart-delivery", queue_restart), ("double-escape-abort", double_escape),
                     ("starter", starter), ("starter-management", starter_management)]
            for name, run in steps:
                check(name, run)
            clients[-1].click("window-close")
            clients[-1].process.wait(timeout=15)
    except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration, subprocess.SubprocessError,
            http.client.HTTPException) as error:
        if not results or results[-1]["passed"]:
            traceback.print_exc()
            results.append({"check": "native-queue", "passed": False, "error": str(error)})
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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--root", type=Path, default=runner.REPO_ROOT / "tmp/parity")
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    args.root = args.root.resolve()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"native-queue-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists() or not (args.root / "seed").is_dir():
        parser.error("choose a new output directory and an existing parity seed")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
