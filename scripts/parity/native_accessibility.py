"""Observe native controls through Orca on a private, silent test desktop."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_platform import ROOT, binary_info, command, isolated_environment, wait_for
import virtual_display


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
    except (RuntimeError, TimeoutError, LookupError, OSError, subprocess.SubprocessError) as error:
        results.append({"check": "setup", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({"binary": binary, "display": "owned-xvfb", "scale": args.scale, "audiblePlayback": False, "checks": results}, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "native/target/release/openchamber-native")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    args = parser.parse_args()
    args.binary = args.binary.resolve()
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
