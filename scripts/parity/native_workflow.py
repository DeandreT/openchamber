"""Exercise connected native creation, send, stop and draft restart in isolation."""

import argparse
from contextlib import ExitStack
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

from native_adapter import NativeClient, accessibility_enabled, set_accessibility, _atspi
from native_platform import binary_info, command, editable_text, wait_for
from clipboard_probe import ClipboardProbe
import runner
import virtual_display
from workflow_proxy import FaultProxy


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()
    try:
        with ExitStack() as cleanup:
            state = runner.copy_seed(args.root, args.output, "openchamber-dark")
            server = runner.IsolatedServer(args.output, args)
            cleanup.callback(server.stop)
            server.wait_ready(60)
            proxy = None
            if args.fault != "none":
                proxy = FaultProxy(server.url, args.fault, args.fault_operation)
                cleanup.callback(proxy.stop)
            env = runner.native_env(state, proxy.url if proxy else server.url, "openchamber-dark")
            set_accessibility(True)
            clipboard = ClipboardProbe()

            def start(label):
                log = cleanup.enter_context((args.output / f"{label}.log").open("w"))
                client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                cleanup.callback(client.close)
                client.launch()
                client.wait_ready(60, connected=True)
                client.wait_for_marker("composer-usable", 30)
                return client

            def editor():
                nodes = [node for node in client._walk() if node.get_role_name() == "entry"]
                if len(nodes) != 1:
                    raise RuntimeError("expected one editable composer")
                return nodes[0]

            def read_messages():
                url = f"{server.url}/api/session/{urllib.parse.quote(session, safe='')}/message?limit=100"
                with urllib.request.urlopen(url, timeout=10) as response:
                    return json.load(response)["data"]

            def read_sessions():
                with urllib.request.urlopen(f"{server.url}/api/session?limit=100", timeout=10) as response:
                    return json.load(response)["data"]

            def focus_editor():
                client._activate()
                point = []

                def locate_editor():
                    point[:] = client._screen_point(editor())
                    return True

                wait_for("composer did not appear", locate_editor)
                x, y = point
                command(env, "xdotool", "mousemove", str(x), str(y), "click", "1")
                wait_for("composer did not focus", lambda: editor().get_state_set().contains(_atspi().StateType.FOCUSED))

            def posted():
                return [message for message in read_messages() if message.get("type") == "user" and message.get("text") == text]

            def first_prompt_present():
                try:
                    return len(posted()) == 1
                except urllib.error.HTTPError as error:
                    if args.fault_operation == "create" and error.code == 404:
                        # The explicit retry may still be creating its saved ID.
                        return False
                    raise

            def recovery_button(suffix):
                found = [node for node in client._walk() if (node.get_accessible_id() or "").startswith("recovery-") and (node.get_accessible_id() or "").endswith(suffix)]
                if len(found) != 1:
                    raise LookupError("expected one pending-send action")
                return found[0]

            def recovery_visible():
                return any((node.get_accessible_id() or "").endswith("-check") for node in client._walk())

            client = start("first")
            if args.new_session:
                before_sessions = {entry["id"] for entry in read_sessions()}
                client.click("choose-project")
                projects = []

                def read_project_options():
                    projects[:] = [node.get_accessible_id() for node in client._walk()
                                   if (node.get_accessible_id() or "").startswith("project-option:")]
                    return bool(projects)

                wait_for("project chooser did not open", read_project_options)
                directory = projects[0].removeprefix("project-option:")
                client.click(projects[0])
                focus_editor()
            else:
                row = next(row for row in client.rows() if row["label"] == "parity: tools")
                session = row["key"].rsplit(":session:", 1)[1]
                client.click(row["key"])
                if not runner.wait_selected(client, row["key"]):
                    raise RuntimeError("requested session was not selected")
                if not client.wait_for({"marker": "switch-completed"}, 15):
                    raise RuntimeError("selected session did not finish painting")
            wait_for("session composer did not focus", lambda: editor().get_state_set().contains(_atspi().StateType.FOCUSED))
            text = "Native workflow verification 你好 👩‍💻"
            clipboard.paste(text, lambda: command(env, "xdotool", "key", "ctrl+v"), lambda: editable_text(editor()) == text)
            try:
                wait_for("typed prompt did not reach composer", lambda: editable_text(editor()) == text)
            except TimeoutError:
                client.screenshot(args.output / "input-failed.png")
                raise RuntimeError(f"typed prompt did not reach composer; observed {len(editable_text(editor()))} characters")
            wait_for("send control did not enable", lambda: client._find("composer-submit").get_state_set().contains(_atspi().StateType.ENABLED))
            client.key("Return")
            if proxy and args.fault_operation == "create":
                wait_for("fault proxy did not receive creation", lambda: len(proxy.create_ids) == 1, timeout=30)
                session = proxy.create_ids[0]
                wait_for("uncertain creation recovery did not appear", recovery_visible)
                client.screenshot(args.output / "creation-uncertain.png")
                client.click("window-close")
                client.process.wait(timeout=15)
                client.close()
                proxy.pause_events.clear()
                client = start("reopened-creation")
                wait_for("restart lost the uncertain creation", recovery_visible)
                wait_for("creation status check did not enable", lambda: recovery_button("-check").get_state_set().contains(_atspi().StateType.ENABLED))
                client.click(recovery_button("-check").get_accessible_id())
                wait_for("failed creation lookup was not reported", lambda: any(
                    "Couldn't check session creation." in node.get_name() for node in client._walk()))
                if len(proxy.create_ids) != 1 or proxy.prompt_ids:
                    raise RuntimeError("restart or failed lookup repeated creation or sent a prompt")
                proxy.pause_reads.clear()
                client.click(recovery_button("-check").get_accessible_id())
                wait_for("creation retry did not enable", lambda: recovery_button("-retry").get_state_set().contains(_atspi().StateType.ENABLED))
                if proxy.prompt_ids:
                    raise RuntimeError("checking creation sent the first prompt automatically")
                proxy.pause_events.clear()
                client.click(recovery_button("-retry").get_accessible_id())
                wait_for("recovered creation did not send the saved prompt", first_prompt_present, timeout=30)
                wait_for("creation retry did not resolve recovery", lambda: not recovery_visible(), timeout=30)
                expected = 2 if args.fault == "before" else 1
                if proxy.create_ids != [session] * expected or len(proxy.prompt_ids) != 1:
                    raise RuntimeError("creation recovery changed identity or duplicated the first prompt")
                created = [entry for entry in read_sessions() if entry["id"] not in before_sessions]
                if len(created) != 1 or created[0]["id"] != session or created[0].get("location", {}).get("directory") != directory:
                    raise RuntimeError("creation recovery produced the wrong session or project")
                client.screenshot(args.output / "creation-resolved.png")
                client.click("window-close")
                client.process.wait(timeout=15)
                results.append({"check": f"response-loss-{args.fault}-creation", "passed": True})
                return 0
            if args.new_session:
                created = []

                def created_once():
                    created[:] = [entry for entry in read_sessions() if entry["id"] not in before_sessions]
                    if len(created) > 1:
                        raise RuntimeError("one submission created multiple sessions")
                    return len(created) == 1

                wait_for("new session was not created", created_once, timeout=30)
                session = created[0]["id"]
                if created[0].get("location", {}).get("directory") != directory:
                    raise RuntimeError("new session was created outside the selected project")
                results.append({"check": "create-in-selected-project", "passed": True})
            if proxy:
                wait_for("fault proxy did not receive the prompt", lambda: len(proxy.attempt_ids) == 1, timeout=30)
                wait_for("uncertain recovery row did not appear", recovery_visible)
                journal = state / "config/openchamber-native/drafts-and-sends.json"
                wait_for("uncertain send was not retained on disk", lambda: len(json.loads(journal.read_text())["attempts"]) == 1)
                client.screenshot(args.output / "uncertain.png")
                client.click("window-close")
                client.process.wait(timeout=15)
                client.close()
                proxy.pause_events.clear()
                client = start("reopened-uncertain")
                wait_for("restart lost the uncertain send", recovery_visible)
                client.click(recovery_button("-check").get_accessible_id())
                wait_for("failed observation was not reported", lambda: any("Couldn't check delivery." in node.get_name() for node in client._walk()))
                if len(proxy.attempt_ids) != 1:
                    raise RuntimeError("a failed observation retried the mutation")
                proxy.pause_reads.clear()
                client.click(recovery_button("-check").get_accessible_id())
                if args.fault == "after":
                    wait_for("admitted message did not resolve recovery", lambda: not recovery_visible(), timeout=30)
                    if len(proxy.attempt_ids) != 1 or len(posted()) != 1:
                        raise RuntimeError("an admitted prompt was duplicated")
                else:
                    wait_for("unobserved prompt did not allow explicit retry", lambda: recovery_button("-retry").get_state_set().contains(_atspi().StateType.ENABLED))
                    client.click(recovery_button("-retry").get_accessible_id())
                    wait_for("explicit retry did not admit the prompt", lambda: len(posted()) == 1, timeout=30)
                    wait_for("retry did not resolve recovery", lambda: not recovery_visible(), timeout=30)
                    if len(proxy.attempt_ids) != 2 or proxy.attempt_ids[0] != proxy.attempt_ids[1]:
                        raise RuntimeError("retry did not reuse the exact message identity")
                client.screenshot(args.output / "resolved.png")
                client.click("window-close")
                client.process.wait(timeout=15)
                results.append({"check": f"response-loss-{args.fault}-admission", "passed": True})
                return 0
            wait_for("submitted text was not admitted", lambda: len(posted()) == 1, timeout=30)
            wait_for("submitted draft did not clear", lambda: editable_text(editor()) == "")
            wait_for("session never became active", lambda: client._find("composer-submit").get_name() == "Stop generating", timeout=30)
            client.screenshot(args.output / "active.png")
            client.click("composer-submit")
            wait_for("stop did not settle the session", lambda: client._find("composer-submit").get_name() == "Send message", timeout=30)
            if len(posted()) != 1:
                raise RuntimeError("one send created more than one user message")
            results.append({"check": "send-and-stop", "passed": True})

            draft = "Restart-safe draft 你好 👩‍💻"
            focus_editor()
            clipboard.paste(draft, lambda: command(env, "xdotool", "key", "ctrl+v"), lambda: editable_text(editor()) == draft)
            wait_for("draft was not entered", lambda: editable_text(editor()) == draft)
            client.click("window-close")
            client.process.wait(timeout=15)
            if client.process.returncode != 0:
                raise RuntimeError("normal close failed")
            client.close()
            client = start("reopened")
            row = next(row for row in client.rows() if row["key"].endswith(f":session:{session}"))
            client.click(row["key"])
            wait_for("saved draft did not restore", lambda: editable_text(editor()) == draft)
            if len(posted()) != 1:
                raise RuntimeError("restart resubmitted a prompt")
            if args.new_session and {entry["id"] for entry in read_sessions()} != before_sessions | {session}:
                raise RuntimeError("restart changed the created session identity")
            client.screenshot(args.output / "restored.png")
            client.click("window-close")
            client.process.wait(timeout=15)
            results.append({"check": "draft-restart", "passed": True})
    except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration, subprocess.SubprocessError) as error:
        traceback.print_exc()
        results.append({"check": "workflow", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({"binary": binary_info(args.binary), "display": "owned-xvfb", "scale": args.scale, "checks": results}, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--root", type=Path, default=runner.REPO_ROOT / "tmp/parity")
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--fault", choices=("none", "before", "after"), default="none")
    parser.add_argument("--fault-operation", choices=("prompt", "create"), default="prompt")
    parser.add_argument("--new-session", action="store_true", help="create a session in a chosen project before the first prompt")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.fault_operation == "create" and (not args.new_session or args.fault == "none"):
        parser.error("creation response loss requires --new-session and --fault before or after")
    args.root = args.root.resolve()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or runner.REPO_ROOT / "native/target/tmp" / f"native-workflow-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists() or not (args.root / "seed").is_dir():
        parser.error("choose a new output directory and an existing parity seed")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == '__main__':
    sys.exit(main())
