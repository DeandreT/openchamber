"""An owned Xvfb display and private D-Bus session for visual checks.

The test session has no activatable desktop services. Xvfb permits XTEST
pointer input while keeping windows and input away from the user's desktop.
"""
from __future__ import annotations

import argparse
from contextlib import ExitStack
import os
import re
import secrets
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

INSIDE = "OPENCHAMBER_PARITY_DISPLAY"
XVFB_PID = "OPENCHAMBER_PARITY_XVFB_PID"
MARGIN = 640
SHUTDOWN_GRACE_S = 10.0
AT_SPI_LAUNCHER = Path("/usr/lib/at-spi-bus-launcher")
AT_SPI_REGISTRY = Path("/usr/lib/at-spi2-registryd")
BUS_CONFIG = """<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
  "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type><keep_umask/>
  <listen>unix:tmpdir={work}</listen><auth>EXTERNAL</auth>
  <servicedir>{services}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/><allow own="*"/>
  </policy>
</busconfig>
"""
SESSION_VARIABLES = (
    "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS", "XDG_SESSION_TYPE",
)


def inside() -> bool:
    if INSIDE not in os.environ:
        return False
    verify_owned_display()
    return True


def verify_owned_display() -> None:
    """Refuses inherited or stale display handles before a client launches."""
    pid = int(os.environ.get(XVFB_PID, "0"))
    if pid <= 0:
        raise RuntimeError("the visual test does not own an Xvfb process")
    process = Path(f"/proc/{pid}")
    args = (process / "cmdline").read_bytes().split(b"\0")
    display = os.environ.get("DISPLAY", "").encode()
    if process.stat().st_uid != os.getuid() or not args or Path(os.fsdecode(args[0])).name != "Xvfb" or display not in args:
        raise RuntimeError("the configured display is not the owned Xvfb process")
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    if runtime.stat().st_uid != os.getuid() or stat.S_IMODE(runtime.stat().st_mode) != 0o700:
        raise RuntimeError("the visual test runtime directory is not private")
    if any(key in os.environ for key in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XDG_SESSION_TYPE")):
        raise RuntimeError("desktop session variables leaked into the visual test")
    if Path(os.environ["XAUTHORITY"]).parent != runtime:
        raise RuntimeError("the X authority file is outside the private runtime")
    if not os.environ.get("DBUS_SESSION_BUS_ADDRESS", "").startswith(f"unix:path={runtime}/"):
        raise RuntimeError("the accessibility bus is outside the private runtime")


def child_environment(source: dict[str, str]) -> dict[str, str]:
    """Keep profile isolation while forcing all display handles to our own."""
    verify_owned_display()
    env = {key: value for key, value in source.items() if key not in SESSION_VARIABLES}
    for key in ("DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS", INSIDE, XVFB_PID):
        if key in os.environ:
            env[key] = os.environ[key]
    return env


def available() -> str | None:
    for tool in ("Xvfb", "xauth", "xdpyinfo", "dbus-run-session", "busctl", "fc-match", "xsettingsd"):
        if shutil.which(tool) is None:
            return f"{tool} is not installed"
    for daemon in (AT_SPI_LAUNCHER, AT_SPI_REGISTRY):
        if not daemon.exists():
            return f"{daemon} is not installed"
    return None


def stop_process(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=SHUTDOWN_GRACE_S)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def run_inside(command: list[str], width: int, height: int) -> int:
    base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="openchamber-parity-display-", dir=base) as temporary, ExitStack() as cleanup:
        work = Path(temporary)
        work.chmod(0o700)
        env = {key: value for key, value in os.environ.items() if key not in SESSION_VARIABLES}
        env.update({INSIDE: "1", "XDG_RUNTIME_DIR": str(work), "XAUTHORITY": str(work / "Xauthority")})
        with (work / "xvfb.log").open("wb") as log:
            xvfb = None
            for _ in range(20):
                number = 1000 + secrets.randbelow(20000)
                if Path(f"/tmp/.X{number}-lock").exists() or Path(f"/tmp/.X11-unix/X{number}").exists():
                    continue
                display = f":{number}"
                subprocess.run(["xauth", "-f", env["XAUTHORITY"], "source", "-"],
                    input=f"add {display} . {secrets.token_hex(16)}\n", text=True,
                    check=True, capture_output=True)
                env["DISPLAY"] = display
                xvfb = subprocess.Popen([
                    "Xvfb", display, "-screen", "0", f"{width + MARGIN}x{height + MARGIN}x24",
                    "-nolisten", "tcp", "-auth", env["XAUTHORITY"],
                ], env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
                cleanup.callback(stop_process, xvfb)
                deadline = time.monotonic() + 10
                ready = False
                while xvfb.poll() is None and time.monotonic() < deadline:
                    probe = subprocess.run(["xdpyinfo"], env=env, capture_output=True, timeout=2)
                    if probe.returncode == 0:
                        ready = True
                        break
                    time.sleep(0.05)
                if ready:
                    break
                stop_process(xvfb)
                xvfb = None
            if xvfb is None:
                raise RuntimeError("could not start a private Xvfb display")
            env[XVFB_PID] = str(xvfb.pid)
            services = work / "services"
            services.mkdir()
            bus_config = work / "bus.conf"
            bus_config.write_text(BUS_CONFIG.format(work=work, services=services))
            bus_record = work / "bus"
            xsettings = work / "xsettingsd.conf"
            xsettings.write_text(f'Gtk/FontName "{system_ui_family()}, 10"\n')
            script = work / "session.sh"
            script.write_text(
                "#!/bin/sh\n"
                f"cd {shlex.quote(os.getcwd())} || exit 1\n"
                f'printf %s "$DBUS_SESSION_BUS_ADDRESS" > {shlex.quote(str(bus_record))}\n'
                f"{AT_SPI_LAUNCHER} --launch-immediately &\n"
                "for i in $(seq 50); do busctl --user status org.a11y.Bus >/dev/null 2>&1 && break; sleep 0.1; done\n"
                f"{AT_SPI_REGISTRY} &\n"
                "for i in $(seq 50); do busctl --user status org.a11y.atspi.Registry >/dev/null 2>&1 && break; sleep 0.1; done\n"
                f"xsettingsd -c {shlex.quote(str(xsettings))} >/dev/null 2>&1 &\n"
                "sleep 0.5\n"
                f"{shlex.join(command)}\n"
            )
            script.chmod(0o700)
            session = None
            try:
                session = subprocess.Popen(["dbus-run-session", f"--config-file={bus_config}", "--", str(script)],
                    env=env, start_new_session=True)
                return session.wait()
            finally:
                if bus_record.exists():
                    stop_attached(bus_record.read_text())
                if session is not None:
                    stop_process(session)
                stop_process(xvfb)


def system_ui_family() -> str:
    family = subprocess.run(["fc-match", "--format=%{family[0]}", "system-ui"],
        check=True, capture_output=True, text=True).stdout.strip()
    if not family:
        raise RuntimeError("fontconfig resolves no family for system-ui")
    return family


def stop_attached(bus_address: str) -> None:
    if not bus_address:
        return
    wanted = f"DBUS_SESSION_BUS_ADDRESS={bus_address}".encode()
    attached_pids = []
    for entry in Path("/proc").iterdir():
        if not re.fullmatch(r"\d+", entry.name):
            continue
        try:
            if entry.stat().st_uid != os.getuid():
                continue
            attached = wanted in (entry / "environ").read_bytes().split(b"\0")
            if attached:
                pid = int(entry.name)
                attached_pids.append(pid)
                os.kill(pid, signal.SIGTERM)
        except (OSError, ProcessLookupError):
            pass
    deadline = time.monotonic() + SHUTDOWN_GRACE_S
    while attached_pids:
        remaining = []
        for pid in attached_pids:
            try:
                entry = Path(f"/proc/{pid}")
                if entry.stat().st_uid == os.getuid() and wanted in (entry / "environ").read_bytes().split(b"\0"):
                    remaining.append(pid)
            except OSError:
                pass
        attached_pids = remaining
        if not attached_pids:
            break
        if time.monotonic() >= deadline:
            for pid in attached_pids:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            break
        time.sleep(0.05)


def main() -> int:
    parser = argparse.ArgumentParser(description="Run a visual check on an owned Xvfb display.")
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("no command")
    problem = available()
    if problem:
        print(f"no private display: {problem}", file=sys.stderr)
        return 1
    return run_inside(command, args.width, args.height)


if __name__ == "__main__":
    sys.exit(main())
