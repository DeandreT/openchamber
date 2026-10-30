"""Qualify native session management against an owned real engine.

Renames, pins, archives, restores and deletes real sessions through the
native window's sidebar menu, and confirms each outcome through the isolated
server's API. One write is failed on purpose through a loopback proxy to show
that the row returns to the server's state and the error is reported.
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
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request

from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from clipboard_probe import ClipboardProbe
from native_platform import binary_info, command, editable_text, wait_for
import runner
import virtual_display
from workflow_proxy import FaultProxy

RENAMED = "Native rename ok"


def take_rule(rules, method, path):
    """Removes and returns the first armed rule matching the request."""
    for index, (rule_method, pattern) in enumerate(rules):
        if rule_method == method and re.fullmatch(pattern, path):
            return rules.pop(index)
    return None


class ActionFaults(FaultProxy):
    """Forwards everything, failing each armed request once with HTTP 500."""

    def __init__(self, upstream):
        super().__init__(upstream, "after", "prompt")
        self.rules = []
        self.failed = []

    def fail_next(self, method, pattern):
        with self.lock:
            self.rules.append((method, pattern))

    def handle(self, handler):
        path = urllib.parse.urlsplit(handler.path).path
        with self.lock:
            rule = take_rule(self.rules, handler.command, path)
            if rule:
                self.failed.append((handler.command, path))
        if rule is None:
            super().handle(handler)
            return
        handler.rfile.read(int(handler.headers.get("Content-Length", 0)))
        body = b'{"error":"injected"}'
        handler.send_response(500)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(body)))
        handler.end_headers()
        handler.wfile.write(body)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()

    def check(name, run):
        try:
            run()
            results.append({"check": name, "passed": True})
        except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration,
                subprocess.SubprocessError, http.client.HTTPException) as error:
            traceback.print_exc()
            results.append({"check": name, "passed": False, "error": str(error)})
            try:
                client.screenshot(args.output / f"{name}-failed.png")
            except Exception:  # noqa: BLE001 - evidence is best effort after a failure
                pass
            raise

    try:
        with ExitStack() as cleanup:
            state = runner.copy_seed(args.root, args.output, "openchamber-dark")
            server = runner.IsolatedServer(args.output, args)
            cleanup.callback(server.stop)
            server.wait_ready(60)
            proxy = ActionFaults(server.url)
            cleanup.callback(proxy.stop)
            env = runner.native_env(state, proxy.url, "openchamber-dark")
            set_accessibility(True)
            clipboard = ClipboardProbe()
            log = cleanup.enter_context((args.output / "native.log").open("w"))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(60, connected=True)
            client.wait_for_marker("composer-usable", 30)

            def api(path, method="GET"):
                request = urllib.request.Request(f"{server.url}{path}", method=method)
                with urllib.request.urlopen(request, timeout=10) as response:
                    return json.load(response)

            def post(path, body):
                request = urllib.request.Request(f"{server.url}{path}", data=json.dumps(body).encode(), method="POST",
                                                 headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=10) as response:
                    response.read()

            def notice():
                try:
                    return client._find("session-notice").get_name() or ""
                except LookupError:
                    return ""

            journal = state / "config/openchamber-native/drafts-and-sends.json"

            def journal_mentions(session):
                try:
                    return session in json.dumps(json.loads(journal.read_text()).get("attempts", []))
                except (OSError, ValueError):
                    return False

            def entries():
                return [node for node in client._walk() if node.get_role_name() == "entry"]

            def focus_editor():
                client._activate()
                point = []

                def locate():
                    nodes = entries()
                    if len(nodes) != 1:
                        return False
                    point[:] = client._screen_point(nodes[0])
                    return True

                wait_for("the composer did not appear", locate)
                command(env, "xdotool", "mousemove", str(point[0]), str(point[1]), "click", "1")

            def record(session):
                try:
                    return api(f"/api/session/{urllib.parse.quote(session, safe='')}")["data"]
                except urllib.error.HTTPError as error:
                    if error.code == 404:
                        return None
                    raise

            def openchamber(session):
                metadata = (record(session) or {}).get("metadata") or {}
                return metadata.get("openchamber") or {}

            def row_key(session):
                keys = [row["key"] for row in client.rows() if row["key"].endswith(f":session:{session}")]
                return keys[0] if keys else None

            def ids():
                return {node.get_accessible_id() or "" for node in client._walk()}

            def names():
                return " | ".join(node.get_name() or "" for node in client._walk())

            def open_menu(session, accessible=False):
                key = row_key(session)
                if key is None:
                    raise LookupError("the session has no sidebar row")
                trigger = f"{key}:menu"
                if accessible:
                    client.activate(trigger)
                else:
                    client.click(trigger)
                wait_for("the session menu did not open", lambda: "session-menu" in ids())

            def choose(action, accessible=False):
                item = f"session-menu-item:{action}"
                wait_for(f"menu item {action} is missing", lambda: item in ids())
                if accessible:
                    client.activate(item)
                else:
                    client.click(item)
                wait_for("the session menu did not close", lambda: "session-menu" not in ids())

            def confirm():
                wait_for("the confirmation did not open", lambda: "session-confirm" in ids())
                client.screenshot(args.output / f"confirm-{len(results)}.png")
                client.click("session-confirm")

            roots = [entry for entry in api("/api/session?limit=200")["data"]
                     if not entry.get("parentID") and not (entry.get("time") or {}).get("archived")]
            listed = [entry["id"] for entry in roots if row_key(entry["id"])]
            if len(listed) < 3:
                raise RuntimeError("the seed needs three listed root sessions")
            target, other, busy = listed[0], listed[1], listed[2]

            def rename():
                key = row_key(target)
                open_menu(target)
                client.screenshot(args.output / "menu-open.png")
                choose("rename")
                wait_for("the rename field did not open", lambda: f"{key}:rename" in ids())
                # Typed: the private display's clipboard paste is not reliable
                # for this field; Unicode rename is covered by the window tests.
                client._activate()
                command(env, "xdotool", "type", "--delay", "40", RENAMED)
                wait_for("the typed title did not reach the field",
                         lambda: any(editable_text(node) == RENAMED for node in entries()))
                client.key("Return")
                wait_for("the server did not store the new title", lambda: (record(target) or {}).get("title") == RENAMED, timeout=20)
                wait_for("the row did not show the new title", lambda: any(row["label"] == RENAMED for row in client.rows()))
                if openchamber(target).get("workTitle", {}).get("by") != "user":
                    raise RuntimeError("the rename was not noted as the user's")

            def pin():
                open_menu(target, accessible=True)
                choose("pin", accessible=True)
                wait_for("the server did not store the pin", lambda: "pinned" in openchamber(target), timeout=20)

            def failed_unpin():
                proxy.fail_next("POST", rf"/api/openchamber/sessions/{re.escape(target)}/metadata")
                open_menu(target)
                choose("unpin")
                wait_for("the unpin write never reached the proxy", lambda: bool(proxy.failed), timeout=20)
                # Toolkit toasts are not on the accessibility bus; the
                # window's status node carries the same text.
                wait_for("the unpin failure was not announced", lambda: notice() == "Couldn't unpin the session", timeout=10)
                client.screenshot(args.output / "unpin-failed.png")
                if "pinned" not in openchamber(target):
                    raise RuntimeError("the server lost the pin")
                open_menu(target)
                if "session-menu-item:unpin" not in ids():
                    raise RuntimeError("the row did not return to pinned after the failure")
                choose("unpin")
                wait_for("the retried unpin did not reach the server", lambda: "pinned" not in openchamber(target), timeout=20)

            def undo():
                open_menu(other)
                choose("archive")
                confirm()
                wait_for("the server did not archive the session", lambda: ((record(other) or {}).get("time") or {}).get("archived"), timeout=20)
                wait_for("the archive notification did not offer Undo", lambda: "session-archive-undo" in ids(), timeout=10)
                client.activate("session-archive-undo")
                client.screenshot(args.output / "archive-undo.png")
                wait_for("Undo did not restore the session", lambda: not ((record(other) or {}).get("time") or {}).get("archived"), timeout=20)
                wait_for("the undone row did not return", lambda: row_key(other) is not None)
                if notice() == "Session restored":
                    raise RuntimeError("a successful Undo announced a restore")

            def archive():
                open_menu(other)
                choose("archive")
                confirm()
                wait_for("the server did not archive the session", lambda: ((record(other) or {}).get("time") or {}).get("archived"), timeout=20)
                wait_for("the archived row stayed in the sidebar", lambda: row_key(other) is None)

            def restore():
                client.click("archive")
                wait_for("the archive page did not open", lambda: "archive-page" in ids())
                wait_for("the archived session is not listed", lambda: f"archive-restore:{other}" in ids())
                client.screenshot(args.output / "archive-page.png")
                client.click(f"archive-restore:{other}")
                wait_for("the server did not restore the session", lambda: not ((record(other) or {}).get("time") or {}).get("archived"), timeout=20)
                wait_for("the restored session left the archive page", lambda: f"archive-restore:{other}" not in ids())
                client.click("archive")
                wait_for("the restored row did not return", lambda: row_key(other) is not None)

            def delete_with_pending_send():
                # The fixture's think-30s model keeps the first turn running,
                # so the second prompt waits in the session's inbox.
                post(f"/api/session/{urllib.parse.quote(busy, safe='')}/model",
                     {"model": {"providerID": "perf", "id": "think-30s"}})
                wait_for("the busy model was not selected", lambda: ((record(busy) or {}).get("model") or {}).get("id") == "think-30s")
                key = row_key(busy)
                client.click(key)
                wait_for("the session did not open", lambda: runner.wait_selected(client, key, 1))
                for text in ("Pending send one", "Pending send two"):
                    focus_editor()
                    command(env, "xdotool", "type", "--delay", "20", text)
                    wait_for("the prompt did not reach the composer",
                             lambda: any(editable_text(node) == text for node in entries()))
                    client.key("Return")
                    # Either a retained attempt or a recovery row is a pending send.
                    wait_for("the send was not retained", lambda: journal_mentions(busy)
                             or any(item.startswith("recovery-") for item in ids()), timeout=30)
                client.screenshot(args.output / "pending-send.png")
                open_menu(busy)
                choose("delete")
                confirm()
                wait_for("the server still has the busy session", lambda: record(busy) is None, timeout=20)
                wait_for("a recovery row outlived the deleted session", lambda: not any(item.startswith("recovery-") for item in ids()), timeout=20)
                wait_for("the journal kept the deleted session's send", lambda: not journal_mentions(busy), timeout=20)
                client.screenshot(args.output / "pending-send-deleted.png")

            def delete():
                open_menu(other)
                choose("delete")
                confirm()
                wait_for("the server still has the session", lambda: record(other) is None, timeout=20)
                wait_for("the deleted row stayed in the sidebar", lambda: row_key(other) is None)
                client.screenshot(args.output / "deleted.png")

            steps = [("rename", rename), ("pin", pin), ("failed-unpin-recovers", failed_unpin),
                     ("undo", undo), ("archive", archive), ("restore", restore), ("delete", delete)]
            # Depends on the send path's deletion cleanup; it runs last.
            if not args.skip_pending_send:
                steps.append(("delete-with-pending-send", delete_with_pending_send))
            for name, run in steps:
                check(name, run)
            client.click("window-close")
            client.process.wait(timeout=15)
    except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, StopIteration, subprocess.SubprocessError) as error:
        if not results or results[-1]["passed"]:
            traceback.print_exc()
            results.append({"check": "session-management", "passed": False, "error": str(error)})
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
    parser.add_argument("--skip-pending-send", action="store_true",
                        help="leave out the step that needs the send path's deletion cleanup")
    args = parser.parse_args()
    args.root = args.root.resolve()
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"native-session-management-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
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
