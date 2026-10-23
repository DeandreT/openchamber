"""A private display for visual runs.

`run` re-executes itself inside a headless KWin (`kwin_wayland --virtual`)
with its own XWayland server and its own D-Bus session, so the clients under
test share no screen, input, focus or accessibility bus with the user's
desktop: nothing the user does reaches them, and nothing they do reaches the
user. Captures, AT-SPI reads and window activation all happen inside it.

Chromium takes its `system-ui` font from GTK, and GTK from the desktop's
XSETTINGS daemon; without one it falls back to its own default. The private
display publishes the family fontconfig resolves for `system-ui`, which is
what a desktop's settings daemon publishes and what the native client draws.

The private bus activates no services. KWin's startup would otherwise start a
desktop portal and a secret service on it, which outlive the session and ask
the user to unlock their keyring on the real desktop. The session starts the
accessibility bus and its registry itself, and when the run ends every
process attached to the private bus is stopped.
"""

from __future__ import annotations

import argparse
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# Set inside the private display, so a run there does not start another one.
INSIDE = "OPENCHAMBER_PARITY_DISPLAY"
# Room around the clients for client-side shadows and frame lines.
MARGIN = 640
# The private bus: the session bus without any activatable service.
BUS_CONFIG = """<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:tmpdir={work}</listen>
  <auth>EXTERNAL</auth>
  <servicedir>{services}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""
# How long the compositor may take to wind down after the run finished.
SHUTDOWN_GRACE_S = 10.0
# The accessibility bus and its registry. Their D-Bus activation goes through
# the user's systemd, which would attach them to the desktop's session, so the
# private session starts its own.
AT_SPI_LAUNCHER = Path("/usr/lib/at-spi-bus-launcher")
AT_SPI_REGISTRY = Path("/usr/lib/at-spi2-registryd")
# Publishes the GTK font, which Chromium's `system-ui` follows.
XSETTINGS = "xsettingsd"
# How long the settings daemon gets to claim the display before a client starts.
XSETTINGS_START_S = 0.5
# The desktop session's handles; the private display brings its own.
SESSION_VARIABLES = ("WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS")


def inside() -> bool:
    return INSIDE in os.environ


def available() -> str | None:
    """Why the private display cannot start, or None when it can."""
    for tool in ("kwin_wayland", "dbus-run-session", "busctl", "fc-match", XSETTINGS):
        if shutil.which(tool) is None:
            return f"{tool} is not installed"
    for daemon in (AT_SPI_LAUNCHER, AT_SPI_REGISTRY):
        if not daemon.exists():
            return f"{daemon} is not installed"
    return None


def run_inside(command: list[str], width: int, height: int) -> int:
    """Runs `command` inside a private display for clients of `width` by
    `height` and returns its exit code."""
    # The bus listens on a socket in the work directory, and a socket path
    # may not exceed 108 bytes: a long TMPDIR would keep the bus from
    # starting, so the work directory sits in a short, private base.
    base = os.environ.get("XDG_RUNTIME_DIR") or "/tmp"
    with tempfile.TemporaryDirectory(prefix="openchamber-parity-display-", dir=base) as work:
        status = Path(work) / "status"
        session = Path(work) / "session.sh"
        bus_record = Path(work) / "bus"
        # The compositor's own output is noise unless it ends the run early;
        # the run's errors are not.
        run_errors = Path(work) / "stderr"
        compositor_log = Path(work) / "compositor.log"
        services = Path(work) / "services"
        services.mkdir()
        bus_config = Path(work) / "bus.conf"
        bus_config.write_text(BUS_CONFIG.format(work=work, services=services))
        xsettings = Path(work) / "xsettingsd.conf"
        xsettings.write_text(f'Gtk/FontName "{system_ui_family()}, 10"\n')
        session.write_text(
            "#!/bin/sh\n"
            # The compositor does not keep the caller's working directory.
            f"cd {shlex.quote(os.getcwd())} || exit 1\n"
            f'printf %s "$DBUS_SESSION_BUS_ADDRESS" > {shlex.quote(str(bus_record))}\n'
            f"{shlex.quote(str(AT_SPI_LAUNCHER))} --launch-immediately &\n"
            "for _ in $(seq 50); do\n"
            "  busctl --user status org.a11y.Bus >/dev/null 2>&1 && break\n"
            "  sleep 0.1\n"
            "done\n"
            f"{shlex.quote(str(AT_SPI_REGISTRY))} &\n"
            "for _ in $(seq 50); do\n"
            "  busctl --user status org.a11y.atspi.Registry >/dev/null 2>&1 && break\n"
            "  sleep 0.1\n"
            "done\n"
            f"{XSETTINGS} -c {shlex.quote(str(xsettings))} 2>/dev/null &\n"
            f"sleep {XSETTINGS_START_S}\n"
            f"{shlex.join(command)} 2>{shlex.quote(str(run_errors))}\n"
            f'printf %s "$?" > {shlex.quote(str(status))}\n'
        )
        session.chmod(0o700)
        env = {key: value for key, value in os.environ.items() if key not in SESSION_VARIABLES}
        env[INSIDE] = "1"
        log = open(compositor_log, "wb")
        compositor = subprocess.Popen(
            [
                "dbus-run-session", f"--config-file={bus_config}", "--",
                "kwin_wayland", "--virtual", "--xwayland",
                "--width", str(width + MARGIN), "--height", str(height + MARGIN),
                "--socket", f"openchamber-parity-{os.getpid()}",
                "--exit-with-session", str(session),
            ],
            env=env,
            stdin=subprocess.DEVNULL,
            stderr=log,
            start_new_session=True,
        )
        try:
            while compositor.poll() is None and not status.exists():
                time.sleep(0.2)
            deadline = time.monotonic() + SHUTDOWN_GRACE_S
            while compositor.poll() is None and time.monotonic() < deadline:
                time.sleep(0.2)
        except KeyboardInterrupt:
            pass
        finally:
            if compositor.poll() is None:
                os.killpg(compositor.pid, signal.SIGTERM)
                try:
                    compositor.wait(timeout=SHUTDOWN_GRACE_S)
                except subprocess.TimeoutExpired:
                    os.killpg(compositor.pid, signal.SIGKILL)
                    compositor.wait()
            if bus_record.exists():
                stop_attached(bus_record.read_text())
        if run_errors.exists():
            sys.stderr.write(run_errors.read_text(errors="replace"))
        log.close()
        if not status.exists():
            tail = compositor_log.read_text(errors="replace").splitlines()[-20:]
            print("the private display ended before the run finished; the compositor's last output:", file=sys.stderr)
            for line in tail:
                print(f"  {line}", file=sys.stderr)
            return 1
        return int(status.read_text() or 1)


def system_ui_family() -> str:
    """The family fontconfig substitutes for `system-ui`."""
    family = subprocess.run(
        ["fc-match", "--format=%{family[0]}", "system-ui"], check=True, capture_output=True, text=True,
    ).stdout.strip()
    if not family:
        raise RuntimeError("fontconfig resolves no family for system-ui")
    return family


def stop_attached(bus_address: str) -> None:
    """Stops every process of this user still attached to the private bus."""
    if not bus_address:
        return
    wanted = f"DBUS_SESSION_BUS_ADDRESS={bus_address}".encode()
    for entry in Path("/proc").iterdir():
        if not re.fullmatch(r"\d+", entry.name):
            continue
        try:
            attached = wanted in (entry / "environ").read_bytes().split(b"\0")
        except OSError:
            continue
        if attached:
            try:
                os.kill(int(entry.name), signal.SIGTERM)
            except ProcessLookupError:
                pass


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Runs a command in the private display, for probes outside the runner.",
    )
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    parser.add_argument("command", nargs=argparse.REMAINDER, help="-- command [arguments]")
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
