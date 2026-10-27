"""Qualify native Linux selection, window controls and IBus on an owned display."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient, _atspi, accessibility_enabled, set_accessibility
import virtual_display

ROOT = Path(__file__).resolve().parents[2]


def binary_info(path):
    digest = hashlib.sha256()
    with path.open("rb") as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"path": str(path), "sha256": digest.hexdigest()}


def editable_text(node):
    text = node.get_text_iface()
    if text is None:
        raise RuntimeError("the composer exposes no accessible text interface")
    # GI's Accessible.get_text() is the interface getter and shadows Text.get_text.
    return _atspi().Text.get_text(text, 0, -1)


def check_text_extents(node, text):
    """Visible short CJK input must expose nonempty character geometry."""
    api = _atspi()
    interface = node.get_text_iface()
    if interface is None:
        raise RuntimeError("the composer exposes no accessible text interface")
    area = node.get_extents(api.CoordType.WINDOW)
    for offset in range(len(text)):
        box = api.Text.get_character_extents(interface, offset, api.CoordType.WINDOW)
        if (box.width <= 0 or box.height <= 0 or box.x < area.x - 1 or box.y < area.y - 1
                or box.x + box.width > area.x + area.width + 1
                or box.y + box.height > area.y + area.height + 1):
            raise RuntimeError(f"a committed character has missing or misplaced accessible bounds: character=({box.x}, {box.y}, {box.width}, {box.height}), editor=({area.x}, {area.y}, {area.width}, {area.height})")


def command(env, *args, timeout=20):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip()}")
    return result.stdout


def wait_for(description, check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if check():
                return
        except (RuntimeError, LookupError, subprocess.TimeoutExpired):
            pass
        time.sleep(0.05)
    raise TimeoutError(description)


def isolated_environment(profile):
    keep = ("PATH", "USER", "LOGNAME", "SHELL", "TMPDIR", "LD_LIBRARY_PATH", "VK_ICD_FILENAMES", "LIBGL_DRIVERS_PATH", "MESA_LOADER_DRIVER_OVERRIDE")
    env = {key: os.environ[key] for key in keep if key in os.environ}
    env.update(HOME=str(profile), LANG="en_US.UTF-8", GSETTINGS_BACKEND="keyfile", RUST_BACKTRACE="1")
    for variable, folder in (("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")):
        target = profile / folder
        target.mkdir(parents=True, exist_ok=True)
        env[variable] = str(target)
    env.update(OPENCHAMBER_NATIVE_WORKLOAD="small", OPENCHAMBER_NATIVE_THEME="openchamber-dark", OPENCHAMBER_NATIVE_LOCALE="en")
    return virtual_display.child_environment(env)


def qualify(args):
    virtual_display.verify_owned_display()
    binary = binary_info(args.binary)
    args.output.mkdir(parents=True, exist_ok=True)
    logs = []
    results = []
    previous_a11y = accessibility_enabled()

    def start(name, argv, env):
        log = (args.output / f"{name}.log").open("w")
        logs.append(log)
        process = subprocess.Popen(argv, env=env, stdout=log, stderr=log, start_new_session=True)
        cleanup.callback(virtual_display.stop_process, process)
        return process

    def check(name, callback):
        try:
            detail = callback()
            results.append({"check": name, "passed": True, "detail": detail})
            print(f"{name}: passed", flush=True)
        except (OSError, RuntimeError, TimeoutError, LookupError, subprocess.SubprocessError) as error:
            results.append({"check": name, "passed": False, "error": str(error)})
            print(f"{name}: failed: {error}", flush=True)

    try:
        with ExitStack() as cleanup:
            directory = cleanup.enter_context(tempfile.TemporaryDirectory(prefix="native-platform-", dir=os.environ["XDG_RUNTIME_DIR"]))
            profile = Path(directory)
            env = isolated_environment(profile)
            rc = profile / "openbox.xml"
            rc.write_text('<?xml version="1.0"?><openbox_config xmlns="http://openbox.org/3.4/rc"><applications><application class="*"><decor>no</decor></application></applications></openbox_config>')
            start("openbox", ["openbox", "--config-file", str(rc)], env)
            wait_for("Openbox did not register", lambda: "WINDOW" in command(env, "xprop", "-root", "_NET_SUPPORTING_WM_CHECK"))

            if "ime" in args.checks:
                env.update(XMODIFIERS="@im=ibus", GTK_IM_MODULE="ibus", QT_IM_MODULE="ibus")
                for key in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME"):
                    (Path(env[key]) / "ibus/libpinyin").mkdir(parents=True)
                for key, value in (("preload-engines", "['libpinyin', 'xkb:us::eng']"), ("engines-order", "['libpinyin', 'xkb:us::eng']"), ("enable-by-default", "true")):
                    command(env, "gsettings", "set", "org.freedesktop.ibus.general", key, value)
                command(env, "gsettings", "set", "com.github.libpinyin.ibus-libpinyin.libpinyin", "lua-extension", "false")
                start("ibus", ["ibus-daemon", "--xim", "--panel=disable", "--config=/usr/lib/ibus/ibus-dconf", "--emoji-extension=disable", "--cache=none"], env)
                wait_for("IBus did not register libpinyin", lambda: "libpinyin" in command(env, "ibus", "list-engine"))
                env["IBUS_ADDRESS"] = command(env, "ibus", "address").strip()
                start("libpinyin", ["/usr/lib/ibus-libpinyin/ibus-engine-libpinyin", "--ibus"], env)

            clipboard = ClipboardProbe()
            set_accessibility(True)
            native_log = (args.output / "native.log").open("w")
            logs.append(native_log)
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=native_log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(30, connected=False)
            row = next(row for row in client.rows() if ":session:" in row["key"])
            client.click(row["key"])
            time.sleep(0.3)

            def set_engine(name):
                command(env, "gdbus", "call", "--address", env["IBUS_ADDRESS"], "--dest", "org.freedesktop.IBus", "--object-path", "/org/freedesktop/IBus", "--method", "org.freedesktop.IBus.SetGlobalEngine", name)
                wait_for("IBus did not select the requested engine", lambda: name in command(env, "gdbus", "call", "--address", env["IBUS_ADDRESS"], "--dest", "org.freedesktop.IBus", "--object-path", "/org/freedesktop/IBus", "--method", "org.freedesktop.IBus.GetGlobalEngine"))

            def editor():
                entries = [node for node in client._walk() if node.get_role_name() == "entry"]
                if len(entries) != 1:
                    raise LookupError("expected exactly one composer entry")
                return entries[0]

            def input_text():
                return editable_text(editor())

            def copy_input():
                def dispatch():
                    client._activate()
                    command(env, "xdotool", "key", "ctrl+a", "ctrl+c")
                return clipboard.copy(dispatch)

            def selection():
                rich = next(row for row in client.rows() if row["key"].endswith("ses_000003"))
                client.click(rich["key"])
                if not client.wait_for({"marker": "switch-completed"}, 10):
                    raise TimeoutError("the rich transcript did not paint")
                client._activate()
                content = client._content()
                geometry = dict(line.split("=", 1) for line in command(env, "xdotool", "getwindowgeometry", "--shell", client._window()).split())
                x, y = int(geometry["X"]) + content.x, int(geometry["Y"]) + content.y
                def point(left, top):
                    return str(x + round(left * args.scale)), str(y + round(top * args.scale))
                # Fixed points in the 1280x800 synthetic fixture select across
                # the final two replies, table and Unicode text included.
                command(env, "xdotool", "mousemove", *point(299, 335), "mousedown", "1")
                for step in range(1, 21):
                    command(env, "xdotool", "mousemove", *point(299 + step * 29, 335 + step * 12.4))
                command(env, "xdotool", "mouseup", "1")
                def copy():
                    client._activate()
                    return clipboard.copy(lambda: command(env, "xdotool", "key", "ctrl+c"))
                expected = copy()
                if not all(text in expected for text in ("stream is live-only", "Turn 119", "👩‍💻")):
                    raise RuntimeError("pointer selection missed the expected cross-message Unicode text")
                client.screenshot(args.output / "selection-visible.png")
                command(env, "xdotool", "mousemove", *point(700, 400), "click", "--repeat", "30", "--delay", "20", "4")
                def has_tail():
                    return any("Turn 119 keeps both" in node.get_name() for node in client._walk())
                wait_for("selected rows did not leave the accessibility tree", lambda: not has_tail())
                if copy() != expected:
                    raise RuntimeError("copy changed while the selected rows were offscreen")
                client.screenshot(args.output / "selection-offscreen.png")
                command(env, "xdotool", "click", "--repeat", "30", "--delay", "20", "5")
                wait_for("selected rows did not return", has_tail)
                if copy() != expected:
                    raise RuntimeError("copy changed after selected rows returned")
                client.screenshot(args.output / "selection-returned.png")
                client.click(row["key"])
                if not client.wait_for({"marker": "switch-completed"}, 10):
                    raise TimeoutError("the original transcript did not paint")
                return "Pointer-selected Unicode text copied identically while visible, offscreen and restored."

            if "selection" in args.checks:
                check("selection", selection)

            def ime():
                client._activate()
                x, y = client._screen_point(editor())
                command(env, "xdotool", "mousemove", str(x), str(y), "click", "1")
                wait_for("composer did not receive keyboard focus", lambda: editor().get_state_set().contains(_atspi().StateType.FOCUSED))
                set_engine("libpinyin")
                command(env, "xdotool", "type", "--clearmodifiers", "nihao")
                wait_for("the composition did not appear", lambda: bool(input_text()))
                command(env, "xdotool", "key", "Return")
                wait_for("Enter did not preserve the draft", lambda: input_text() == "nihao")
                set_engine("xkb:us::eng")
                raw = copy_input()
                if raw != "nihao":
                    raise RuntimeError(f"IME Enter changed or submitted the draft: {raw!r}")
                command(env, "xdotool", "key", "ctrl+a", "BackSpace")
                wait_for("the first composition did not clear", lambda: input_text() == "")
                set_engine("libpinyin")
                command(env, "xdotool", "type", "--clearmodifiers", "nihao")
                wait_for("the second composition did not appear", lambda: bool(input_text()))
                command(env, "xdotool", "key", "space")
                wait_for("Space did not commit the Chinese candidate", lambda: input_text() == "你好")
                check_text_extents(editor(), "你好")
                set_engine("xkb:us::eng")
                text = copy_input()
                client.screenshot(args.output / "ime-committed.png")
                if text != "你好":
                    raise RuntimeError(f"IME committed {text!r}, expected '你好'")
                return "Enter committed the composition without submitting; Space committed the Chinese candidate."

            if "ime" in args.checks:
                check("ime", ime)

            def windows():
                time.sleep(2)
                original = client._content()
                original_size = (original.width, original.height)
                def state():
                    return command(env, "xprop", "-id", client._window(), "_NET_WM_STATE")
                client.click("window-maximize")
                wait_for("window did not maximize", lambda: "MAXIMIZED_VERT" in state() and "MAXIMIZED_HORZ" in state())
                client.click("window-maximize")
                wait_for("window did not restore", lambda: "MAXIMIZED_VERT" not in state())
                wait_for("restored content size differs", lambda: (client._content().width, client._content().height) == original_size)
                client.click("window-minimize")
                wait_for("window did not minimize", lambda: "HIDDEN" in state())
                command(env, "xdotool", "windowmap", client._window(), "windowactivate", "--sync", client._window())
                client.click("window-close")
                client.process.wait(timeout=10)
                if client.process.returncode != 0:
                    raise RuntimeError("window close did not exit cleanly")
                if client.substitutions:
                    raise RuntimeError("pointer input required an accessibility substitution")
                return "Maximize, restore, minimize and close reached the window manager."

            if "windows" in args.checks:
                check("windows", windows)
    except (OSError, RuntimeError, TimeoutError, LookupError, subprocess.SubprocessError) as error:
        results.append({"check": "setup", "passed": False, "error": str(error)})
    finally:
        set_accessibility(previous_a11y)
        for log in logs:
            log.close()
        (args.output / "report.json").write_text(json.dumps({"binary": binary, "display": "owned-xvfb", "scale": args.scale, "checks": results}, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "native/target/release/openchamber-native")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--checks", type=lambda text: text.split(","), default=["selection", "ime", "windows"])
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    args = parser.parse_args()
    if set(args.checks) - {"windows", "ime", "selection"}:
        parser.error("checks must be selection, ime, windows, or a comma-separated combination")
    args.binary = args.binary.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be an executable file")
    args.output = (args.output or ROOT / "native/target/tmp" / f"native-platform-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists() and (not args.output.is_dir() or any(args.output.iterdir())):
        parser.error("the output directory is not empty; choose a new directory")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
