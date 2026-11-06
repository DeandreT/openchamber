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
import select
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
INPUT_SESSION_VARIABLES = (
    "IBUS_ADDRESS", "IBUS_USE_PORTAL", "XMODIFIERS", "GTK_IM_MODULE", "QT_IM_MODULE",
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
    if not callable(getattr(os, "pidfd_open", None)) or not callable(getattr(signal, "pidfd_send_signal", None)):
        return "pidfd process ownership is unavailable"
    descriptor = None
    try:
        descriptor = os.pidfd_open(os.getpid())
        signal.pidfd_send_signal(descriptor, 0)
    except OSError:
        return "pidfd process ownership is unavailable"
    finally:
        if descriptor is not None:
            os.close(descriptor)
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
    if problem := available():
        raise RuntimeError(f"cannot start a private display: {problem}")
    stopping = False
    def request_stop(_signal, _frame):
        nonlocal stopping
        stopping = True
    previous = signal.signal(signal.SIGTERM, request_stop)
    try:
        return _run_inside(command, width, height, lambda: stopping)
    finally:
        signal.signal(signal.SIGTERM, previous)


def wait_for_display(xvfb, env, stopping) -> bool:
    """A timed-out probe is not readiness; retry only inside the startup budget."""
    deadline = time.monotonic() + 10
    while xvfb.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or stopping():
            return False
        try:
            probe = subprocess.run(["xdpyinfo"], env=env, capture_output=True, timeout=min(2, remaining))
            if probe.returncode == 0:
                return True
        except subprocess.TimeoutExpired:
            pass
        time.sleep(0.05)
    return False


def _run_inside(command: list[str], width: int, height: int, stopping) -> int:
    base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="openchamber-parity-display-", dir=base) as temporary, ExitStack() as cleanup:
        work = Path(temporary)
        work.chmod(0o700)
        env = {key: value for key, value in os.environ.items() if key not in SESSION_VARIABLES + INPUT_SESSION_VARIABLES}
        env.update({INSIDE: "1", "XDG_RUNTIME_DIR": str(work), "XAUTHORITY": str(work / "Xauthority")})
        env["GSETTINGS_BACKEND"] = "memory"
        with (work / "xvfb.log").open("wb") as log:
            xvfb = None
            for _ in range(20):
                if stopping():
                    return 128 + signal.SIGTERM
                number = 1000 + secrets.randbelow(20000)
                if Path(f"/tmp/.X{number}-lock").exists() or Path(f"/tmp/.X11-unix/X{number}").exists():
                    continue
                display = f":{number}"
                subprocess.run(["xauth", "-f", env["XAUTHORITY"], "source", "-"],
                    input=f"add {display} . {secrets.token_hex(16)}\n", text=True,
                    check=True, capture_output=True, timeout=5)
                env["DISPLAY"] = display
                xvfb = subprocess.Popen([
                    "Xvfb", display, "-screen", "0", f"{width + MARGIN}x{height + MARGIN}x24",
                    "-nolisten", "tcp", "-auth", env["XAUTHORITY"],
                ], env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
                cleanup.callback(stop_process, xvfb)
                ready = wait_for_display(xvfb, env, stopping)
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
                if stopping():
                    return 128 + signal.SIGTERM
                session = subprocess.Popen(["dbus-run-session", f"--config-file={bus_config}", "--", str(script)],
                    env=env, start_new_session=True)
                while not stopping():
                    try:
                        return session.wait(timeout=0.2)
                    except subprocess.TimeoutExpired:
                        pass
                return 128 + signal.SIGTERM
            finally:
                try:
                    if bus_record.exists():
                        stop_attached(bus_record.read_text())
                finally:
                    if session is not None:
                        stop_process(session)
                    stop_process(xvfb)


def system_ui_family() -> str:
    family = subprocess.run(["fc-match", "--format=%{family[0]}", "system-ui"],
        check=True, capture_output=True, text=True).stdout.strip()
    if not family:
        raise RuntimeError("fontconfig resolves no family for system-ui")
    return family


def _attached_start_ticks(entry: Path, wanted: bytes) -> int | None:
    try:
        if entry.stat().st_uid != os.getuid() or wanted not in (entry / "environ").read_bytes().split(b"\0"):
            return None
        fields = (entry / "stat").read_text().rsplit(") ", 1)[1].split()
        return int(fields[19])
    except (OSError, ValueError, IndexError):
        return None


def stop_attached(bus_address: str) -> None:
    if not bus_address:
        return
    wanted = f"DBUS_SESSION_BUS_ADDRESS={bus_address}".encode()
    with ExitStack() as cleanup:
        attached = []
        for entry in Path("/proc").iterdir():
            if not re.fullmatch(r"\d+", entry.name) or int(entry.name) <= 0:
                continue
            start = _attached_start_ticks(entry, wanted)
            if start is None:
                continue
            try:
                descriptor = os.pidfd_open(int(entry.name))
            except ProcessLookupError:
                continue
            if _attached_start_ticks(entry, wanted) != start:
                os.close(descriptor)
                continue
            cleanup.callback(os.close, descriptor)
            try:
                signal.pidfd_send_signal(descriptor, signal.SIGTERM)
                attached.append(descriptor)
            except ProcessLookupError:
                pass
        poller = select.poll()
        for descriptor in attached:
            poller.register(descriptor, select.POLLIN)
        deadline = time.monotonic() + SHUTDOWN_GRACE_S
        while attached:
            exited = {descriptor for descriptor, _ in poller.poll(0)}
            for descriptor in exited:
                poller.unregister(descriptor)
            attached = [descriptor for descriptor in attached if descriptor not in exited]
            if not attached:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                for descriptor in attached:
                    try:
                        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                break
            poller.poll(max(1, int(remaining * 1000)))


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
