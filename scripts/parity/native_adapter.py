"""Native side of the parity runner: the GPUI window through AT-SPI.

Targets are found by accessible ID, which the desktop sets to the same row
keys the web sidebar puts in `data-sidebar-row-key`. Pointer input goes
through AT-SPI's generated X input at the target's centre, keyboard input
through generated key events, and accessibility activation through the
target's AT-SPI action; each path is a separate scenario variant. The
client area is captured with spectacle (KDE) while the window is active.
"""

from __future__ import annotations

import json
import os
import selectors
import signal
import subprocess
import time
from pathlib import Path

MARK_PREFIX = "OPENCHAMBER_NATIVE_MARK "


def _atspi():
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi

    return Atspi


def accessibility_enabled() -> bool:
    output = subprocess.run(
        ["busctl", "--user", "get-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled"],
        capture_output=True, text=True, check=True,
    ).stdout
    return output.strip().endswith("true")


def set_accessibility(enabled: bool) -> None:
    """AccessKit registers with AT-SPI only while the session flag is on."""
    subprocess.run(
        ["busctl", "--user", "set-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled", "b", "true" if enabled else "false"],
        check=True,
    )


class NativeClient:
    """One native desktop process under test."""

    def __init__(self, binary: Path, env: dict[str, str], scale: float, width: int, height: int):
        self.binary = binary
        self.env = env
        self.scale = scale
        self.width = width
        self.height = height
        self.process: subprocess.Popen | None = None
        self.marks: list[dict] = []
        self._selector = selectors.DefaultSelector()
        self._buffer = ""
        self._app = None

    def launch(self) -> dict:
        env = dict(self.env)
        env.pop("WAYLAND_DISPLAY", None)
        env.update(
            XDG_SESSION_TYPE="x11",
            GPUI_X11_SCALE_FACTOR=str(self.scale),
            OPENCHAMBER_NATIVE_DIAGNOSTICS="1",
        )
        self.process = subprocess.Popen(
            [str(self.binary)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self._selector.register(self.process.stdout, selectors.EVENT_READ)
        return {"pid": self.process.pid}

    # --- readiness -------------------------------------------------------

    def _read_marks(self, timeout_s: float) -> None:
        for key, _ in self._selector.select(timeout_s):
            chunk = os.read(key.fd, 65536)
            if not chunk:
                return
            self._buffer += chunk.decode("utf-8", "replace")
            *lines, self._buffer = self._buffer.split("\n")
            for line in lines:
                if line.startswith(MARK_PREFIX):
                    self.marks.append(json.loads(line[len(MARK_PREFIX):]))

    def wait_for_marker(self, marker: str, timeout_s: float) -> dict:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            for mark in self.marks:
                if mark["marker"] == marker:
                    return mark
            if self.process is not None and self.process.poll() is not None:
                raise RuntimeError(f"the native client exited before {marker}")
            self._read_marks(0.05)
        raise TimeoutError(f"no {marker} marker within {timeout_s} s")

    def wait_ready(self, timeout_s: float, connected: bool) -> None:
        """The first content frame, and with a server the painted session list."""
        self.wait_for_marker("content-painted", timeout_s)
        if connected:
            self.wait_for_marker("sessions-shown", timeout_s)
        self._app = self._find_app(timeout_s)
        # A painted frame after readiness: markers are emitted at paint.
        self._read_marks(0.3)

    # --- accessibility ---------------------------------------------------

    def _find_app(self, timeout_s: float):
        atspi = _atspi()
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            desktop = atspi.get_desktop(0)
            for index in range(desktop.get_child_count()):
                app = desktop.get_child_at_index(index)
                # By process: another copy of the app can be on the bus.
                if app is not None and app.get_process_id() == self.process.pid:
                    return app
            time.sleep(0.1)
        raise TimeoutError("the native window never reached the accessibility bus")

    def _walk(self):
        stack = [(self._app, 0)]
        while stack:
            node, depth = stack.pop()
            if node is None or depth > 24:
                continue
            yield node
            for index in reversed(range(node.get_child_count())):
                stack.append((node.get_child_at_index(index), depth + 1))

    def _find(self, accessible_id: str):
        matches = [node for node in self._walk() if (node.get_accessible_id() or "") == accessible_id]
        if len(matches) != 1:
            raise LookupError(f"{len(matches)} native elements have the ID {accessible_id!r}")
        return matches[0]

    def _rect(self, node) -> list[float]:
        atspi = _atspi()
        extents = node.get_extents(atspi.CoordType.WINDOW)
        # AT-SPI reports physical pixels; geometry compares logical ones.
        return [extents.x / self.scale, extents.y / self.scale, extents.width / self.scale, extents.height / self.scale]

    def rows(self) -> list[dict]:
        """Sidebar rows in display order, as `electron_bridge.mjs` reports them."""
        atspi = _atspi()
        sidebar = self._find("sidebar")
        rows = []
        stack = [sidebar]
        while stack:
            node = stack.pop()
            for index in reversed(range(node.get_child_count())):
                stack.append(node.get_child_at_index(index))
            accessible_id = node.get_accessible_id() or ""
            if node is sidebar or not accessible_id or accessible_id == "connection-status":
                continue
            states = node.get_state_set()
            expandable = states.contains(atspi.StateType.EXPANDABLE)
            rows.append({
                "key": accessible_id,
                "label": node.get_name() or "",
                "selected": states.contains(atspi.StateType.SELECTED),
                "expanded": states.contains(atspi.StateType.EXPANDED) if expandable else None,
                "rect": self._rect(node),
            })
        rows.sort(key=lambda row: row["rect"][1])
        return rows

    def rects(self, targets: dict[str, str]) -> dict[str, list[float] | None]:
        result = {}
        for name, accessible_id in targets.items():
            try:
                result[name] = self._rect(self._find(accessible_id))
            except LookupError:
                result[name] = None
        return result

    # --- input -----------------------------------------------------------

    def click(self, accessible_id: str) -> None:
        """A real pointer click at the target's centre."""
        atspi = _atspi()
        extents = self._find(accessible_id).get_extents(atspi.CoordType.SCREEN)
        x = extents.x + extents.width // 2
        y = extents.y + extents.height // 2
        atspi.generate_mouse_event(x, y, "abs")
        time.sleep(0.05)
        atspi.generate_mouse_event(x, y, "b1c")

    def activate(self, accessible_id: str) -> None:
        """The target's accessibility action, as a screen reader invokes it."""
        node = self._find(accessible_id)
        action = node.get_action_iface()
        if action is None or action.get_n_actions() == 0:
            raise LookupError(f"{accessible_id!r} offers no accessibility action")
        action.do_action(0)

    def key(self, name: str) -> None:
        atspi = _atspi()
        from gi.repository import Gdk

        keyval = Gdk.keyval_from_name(name)
        atspi.generate_keyboard_event(keyval, None, atspi.KeySynthType.SYM)

    def type_text(self, text: str) -> None:
        atspi = _atspi()
        atspi.generate_keyboard_event(0, text, atspi.KeySynthType.STRING)

    # --- evidence --------------------------------------------------------

    def screenshot(self, path: Path) -> dict:
        """The active window's client area, without decoration or shadow."""
        subprocess.run(["spectacle", "-b", "-n", "-a", "-e", "-S", "-o", str(path)], check=True, capture_output=True)
        return {"path": str(path)}

    def close(self) -> dict:
        if self.process is None:
            return {}
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=10)
        return {"exitCode": self.process.returncode}
