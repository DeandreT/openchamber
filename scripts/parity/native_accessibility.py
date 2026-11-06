"""Observe native controls through Orca on a private, silent test desktop."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import ROOT, binary_info, command, isolated_environment, wait_for
from native_session_management import ActionFaults
import runner
import virtual_display

PIN_FAILED = "Couldn't pin the session"
# `Atspi.Live.POLITE`, as the web toaster's live region is.
POLITE = 1


def listen_for_announcements(path):
    """Append every AT-SPI `object:announcement` on the bus to `path` as a
    JSON line, independently of Orca. Runs until it is stopped."""
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi

    def record(event):
        try:
            role = event.source.get_role_name()
        except Exception:  # noqa: BLE001 - the source may be gone by now
            role = None
        text = event.any_data if isinstance(event.any_data, str) else None
        with path.open("a") as log:
            log.write(json.dumps({"text": text, "politeness": event.detail1, "role": role}) + "\n")

    Atspi.init()
    listener = Atspi.EventListener.new(record)
    listener.register("object:announcement")
    Atspi.event_main()


def announced(lines, text):
    """The recorded announcements carrying `text`."""
    events = []
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if text.lower() in (event.get("text") or "").lower():
            events.append(event)
    return events


def spoken_count(log, text):
    """How many lines of Orca's debug log speak `text`."""
    return sum("SPEECH OUTPUT:" in line and text.lower() in line.lower() for line in log.splitlines())


def menu_state_problems(items, enabled_actions):
    """What is wrong with `(action, enabled)` pairs read from a session menu:
    exactly `enabled_actions` may expose ENABLED."""
    problems = [f"{action} is {'enabled' if enabled else 'disabled'}"
                for action, enabled in items if enabled != (action in enabled_actions)]
    missing = set(enabled_actions) - {action for action, _ in items}
    return problems + [f"{action} is missing" for action in sorted(missing)]


def qualify(args):
    virtual_display.verify_owned_display()
    binary = binary_info(args.binary)
    args.output.mkdir(parents=True)
    debug_file = args.output / "orca.log"
    results = []
    previous = accessibility_enabled()

    def speech():
        return debug_file.read_text(errors="replace") if debug_file.exists() else ""

    def said(text, after=0, exclude=None):
        return any("SPEECH OUTPUT:" in line and text.lower() in line.lower() and (exclude is None or exclude.lower() not in line.lower()) for line in speech()[after:].splitlines())

    def check(name, callback):
        try:
            callback()
            results.append({"check": name, "passed": True})
            print(f"{name}: passed", flush=True)
            return True
        except (RuntimeError, TimeoutError, LookupError, OSError, subprocess.SubprocessError) as error:
            results.append({"check": name, "passed": False, "error": str(error)})
            print(f"{name}: failed: {error}", flush=True)
            return False

    try:
        with ExitStack() as cleanup:
            profile = Path(cleanup.enter_context(tempfile.TemporaryDirectory(prefix="native-orca-", dir=os.environ["XDG_RUNTIME_DIR"])))
            env = isolated_environment(profile)
            env.update(GSETTINGS_BACKEND="memory", PULSE_SERVER=f"unix:{profile / 'no-audio'}", PIPEWIRE_REMOTE=str(profile / 'no-pipewire'))
            config = profile / "speech-config"
            (config / "modules").mkdir(parents=True)
            (config / "modules/dummy.conf").write_text("")
            (config / "speechd.conf").write_text('LogLevel 5\nAudioOutputMethod "pulse"\nAddModule "dummy" "sd_dummy" "dummy.conf"\nDefaultModule "dummy"\n')
            socket = profile / "speech.sock"
            env["SPEECHD_ADDRESS"] = f"unix_socket:{socket}"

            def start(name, argv):
                log = cleanup.enter_context((args.output / f"{name}.log").open("w"))
                process = subprocess.Popen(argv, env=env, stdout=log, stderr=log, start_new_session=True)
                cleanup.callback(virtual_display.stop_process, process)
                return process

            start("speech", ["speech-dispatcher", "-s", "-c", "unix_socket", "-C", str(config), "-S", str(socket), "-P", str(profile / "speech.pid"), "-L", str(args.output), "-m", "/usr/lib/speech-dispatcher/speech-dispatcher-modules"])
            wait_for("the silent speech service did not start", socket.exists)
            set_accessibility(True)
            announcements = args.output / "announcements.jsonl"
            announcements.touch()
            start("announcement-listener", [sys.executable, str(Path(__file__).resolve()), "--listen-announcements", str(announcements)])
            start("orca-process", ["orca", "--debug-file", str(debug_file)])
            wait_for("Orca did not initialize", lambda: said("Screen reader on"))
            log = cleanup.enter_context((args.output / "native.log").open("w"))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(30, connected=False)
            client._activate()

            def trigger():
                before = len(speech())
                command(env, "xdotool", "key", "shift+Tab")
                wait_for("Orca did not announce the project chooser", lambda: said("Choose project", before))
            check("project-trigger-name", trigger)

            def search():
                before = len(speech())
                client.click("choose-project")
                if not client.wait_for({"named": {"role": "entry", "name": "Search projects..."}}, 10):
                    raise TimeoutError("the project search field did not appear")
                wait_for("Orca did not announce the search field", lambda: said("Search projects", before))

            def option():
                before = len(speech())
                command(env, "xdotool", "key", "Down")
                wait_for("Orca did not announce the highlighted project", lambda: said("openchamber", before, exclude="native"))

            def composer():
                before = len(speech())
                command(env, "xdotool", "key", "Return")
                wait_for("Orca did not announce the composer", lambda: said("Plan, build, or ask", before))
                wait_for("the closed picker did not leave exactly one accessible composer entry", lambda: sum(node.get_role_name() == "entry" for node in client._walk()) == 1)
            steps = [("project-search-focus", search), ("project-option-focus", option), ("composer-focus", composer)]
            for index, (name, callback) in enumerate(steps):
                if not check(name, callback):
                    results.extend({"check": later, "passed": False, "skipped": "previous focus check failed"} for later, _ in steps[index + 1:])
                    break

            def transcript():
                command(env, "xdotool", "key", "Escape")
                row = next(row for row in client.rows() if ":session:" in row["key"])
                client.click(row["key"])
                wait_for("rendered message text is missing from the accessibility tree", lambda: any(node.get_role_name() == "paragraph" and "short answer" in node.get_name().lower() for node in client._walk()))
            check("transcript-text-exposure", transcript)

            def synthetic_menu_states():
                # Without a server only Rename, Copy ID, Pin to and Move to folder can act; the rest must not
                # expose ENABLED to assistive technology.
                import gi

                gi.require_version("Atspi", "2.0")
                from gi.repository import Atspi

                row = next((row for row in client.rows() if ":session:" in row["key"]), None)
                if row is None:
                    raise LookupError("the synthetic sidebar has no session row")
                client.click(f"{row['key']}:menu")
                prefix = "session-menu-item:"
                items = []

                def read():
                    items[:] = [((node.get_accessible_id() or "")[len(prefix):], node.get_state_set().contains(Atspi.StateType.ENABLED))
                                for node in client._walk() if (node.get_accessible_id() or "").startswith(prefix)]
                    return bool(items)
                wait_for("the session menu did not open", read)
                client.screenshot(args.output / "synthetic-session-menu.png")
                (args.output / "synthetic-session-menu.json").write_text(json.dumps(dict(items), indent=2))
                problems = menu_state_problems(items, {"rename", "copy-id", "pin-to", "move-to-folder"})
                command(env, "xdotool", "key", "Escape")
                if problems:
                    raise RuntimeError("; ".join(problems))
            check("synthetic-session-menu-disabled-states", synthetic_menu_states)
            client.close()

            def toast_announcement():
                state = runner.copy_seed(args.root, args.output, "openchamber-dark")
                server = runner.IsolatedServer(args.output, args)
                cleanup.callback(server.stop)
                server.wait_ready(60)
                proxy = ActionFaults(server.url)
                cleanup.callback(proxy.stop)
                connected_env = runner.native_env(state, proxy.url, "openchamber-dark")
                connected_log = cleanup.enter_context((args.output / "native-connected.log").open("w"))
                connected = NativeClient(args.binary, connected_env, args.scale, 1280, 800, stderr=connected_log)
                cleanup.callback(connected.close)
                connected.launch()
                connected.wait_ready(60, connected=True)
                connected._activate()
                row = next((row for row in connected.rows() if ":session:" in row["key"]), None)
                if row is None:
                    raise LookupError("the seed has no session row")
                session = row["key"].rsplit(":session:", 1)[1]
                connected.click(f"{row['key']}:menu")
                wait_for("the Pin to item is missing", lambda: any((node.get_accessible_id() or "") == "session-menu-item:pin-to" for node in connected._walk()))
                connected.click("session-menu-item:pin-to")
                wait_for("the pin item is missing", lambda: any((node.get_accessible_id() or "") == "session-submenu-item:pin" for node in connected._walk()))
                spoken = len(speech())
                proxy.fail_next("POST", rf"/api/openchamber/sessions/{re.escape(session)}/metadata")
                connected.click("session-submenu-item:pin")
                wait_for("the pin write never reached the proxy", lambda: bool(proxy.failed), timeout=20)
                wait_for("no AT-SPI announcement carried the toast text", lambda: announced(announcements.read_text().splitlines(), PIN_FAILED), timeout=10)
                wait_for("Orca did not speak the toast", lambda: said(PIN_FAILED, spoken), timeout=10)
                connected.screenshot(args.output / "pin-failed-toast.png")
                # Through its exit: a closing toast must not speak again.
                wait_for("the toast did not close", lambda: not any(node.get_role_name() == "notification" for node in connected._walk()), timeout=30)
                events = announced(announcements.read_text().splitlines(), PIN_FAILED)
                if len(events) != 1 or events[0]["politeness"] != POLITE:
                    raise RuntimeError(f"expected one polite announcement, got {events}")
                times = spoken_count(speech()[spoken:], PIN_FAILED)
                if times != 1:
                    raise RuntimeError(f"Orca spoke the toast {times} times")
            check("toast-announcement", toast_announcement)
    except (RuntimeError, TimeoutError, LookupError, OSError, subprocess.SubprocessError) as error:
        results.append({"check": "setup", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({"binary": binary, "display": "owned-xvfb", "scale": args.scale, "audiblePlayback": False, "checks": results}, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main():
    if sys.argv[1:2] == ["--listen-announcements"]:
        listen_for_announcements(Path(sys.argv[2]))
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "native/target/release/openchamber-native")
    parser.add_argument("--root", type=Path, default=runner.REPO_ROOT / "tmp/parity", help="parity seed for the connected toast check")
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    args.root = args.root.resolve()
    if not (args.root / "seed").is_dir():
        parser.error("the connected toast check needs an existing parity seed")
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be an executable file")
    args.output = (args.output or ROOT / "native/target/tmp" / f"native-orca-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists():
        parser.error("choose a new output directory")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
