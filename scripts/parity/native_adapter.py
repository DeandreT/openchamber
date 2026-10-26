"""Native side of the parity runner: the GPUI window through AT-SPI.

Targets are found by accessible ID, which the desktop sets to the same row
keys the web sidebar puts in `data-sidebar-row-key`. Pointer input goes
through AT-SPI's generated X input at the target's centre, keyboard input
through generated key events, and accessibility activation through the
target's AT-SPI action; each path is a separate scenario variant. The
client area is captured directly from its X11 window while it is active and
cut to the application's content root, the `workspace` element; geometry is
measured from the same origin, as a web page measures from its viewport.
"""

from __future__ import annotations

import json
import os
import selectors
import signal
import subprocess
import time
from pathlib import Path

import png
import virtual_display
import x11_capture

MARK_PREFIX = "OPENCHAMBER_NATIVE_MARK "
# Physical pixels a client-side frame line may add on each side of the content.
MAX_FRAME_LINE = 4


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

    def __init__(self, binary: Path, env: dict[str, str], scale: float, width: int, height: int, stderr=None):
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
        self._stderr = subprocess.DEVNULL if stderr is None else stderr
        # Markers before this index came before the last input.
        self._input_mark = 0
        # Inputs this host could not deliver and what replaced them.
        self.substitutions: list[dict] = []

    def launch(self) -> dict:
        env = virtual_display.child_environment(self.env)
        env.update(
            GPUI_X11_SCALE_FACTOR=str(self.scale),
            OPENCHAMBER_NATIVE_DIAGNOSTICS="1",
        )
        self.process = subprocess.Popen(
            [str(self.binary)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
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
        self._resize_to_requested(timeout_s)
        # A painted frame after readiness: markers are emitted at paint.
        self._read_marks(0.3)

    def _window(self) -> str:
        windows = subprocess.run(
            ["xdotool", "search", "--pid", str(self.process.pid)], capture_output=True, text=True,
        ).stdout.split()
        if not windows:
            raise RuntimeError("the native client has no X11 window")
        return windows[-1]

    def _resize_to_requested(self, timeout_s: float) -> None:
        """Match physical content dimensions, accounting for the native frame.
        The application's initial-size hold may briefly restore its startup
        dimensions, so verify the result and retry within the readiness limit."""
        target = (round(self.width * self.scale), round(self.height * self.scale))
        deadline = time.monotonic() + timeout_s
        content = None
        while time.monotonic() < deadline:
            try:
                content = self._content()
            except LookupError:
                self._read_marks(0.05)
                continue
            if (content.width, content.height) == target:
                return
            window = self._window()
            geometry = dict(line.split("=", 1) for line in subprocess.run(
                ["xdotool", "getwindowgeometry", "--shell", window],
                check=True, capture_output=True, text=True, timeout=10,
            ).stdout.split())
            width = target[0] + int(geometry["WIDTH"]) - content.width
            height = target[1] + int(geometry["HEIGHT"]) - content.height
            subprocess.run(
                ["xdotool", "windowsize", window, str(width), str(height)],
                check=True, capture_output=True, timeout=10,
            )
            time.sleep(0.1)
        if content is None:
            raise TimeoutError("the native accessibility content never appeared")
        raise TimeoutError(f"native content is {content.width}x{content.height}, wanted {target[0]}x{target[1]}")

    def _activate(self) -> None:
        """Raises and focuses the client's window, so captures of the active
        window and pointer input reach it. The window it opens can lose focus to
        whatever the previous client left active on the private display."""
        virtual_display.verify_owned_display()
        window = self._window()
        subprocess.run(["xdotool", "windowraise", window], check=True, capture_output=True, timeout=10)
        subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True, capture_output=True, timeout=10)

    def _screen_point(self, node) -> tuple[int, int]:
        """The screen position of a node's centre. AccessKit reports screen
        extents relative to the window, so the window's origin comes from X11."""
        atspi = _atspi()
        geometry = dict(
            line.split("=", 1)
            for line in subprocess.run(
                ["xdotool", "getwindowgeometry", "--shell", self._window()], capture_output=True, text=True, check=True,
            ).stdout.split()
        )
        extents = node.get_extents(atspi.CoordType.WINDOW)
        return int(geometry["X"]) + extents.x + extents.width // 2, int(geometry["Y"]) + extents.y + extents.height // 2

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

    def _content(self):
        """Window extents of the application's content root, inside any
        client-side frame the platform draws around it."""
        atspi = _atspi()
        return self._find("workspace").get_extents(atspi.CoordType.WINDOW)

    def _rect(self, node, content=None) -> list[float]:
        """Logical rectangle relative to the content root, as the web page's
        viewport places elements."""
        atspi = _atspi()
        content = content or self._content()
        extents = node.get_extents(atspi.CoordType.WINDOW)
        # AT-SPI reports physical pixels; geometry compares logical ones.
        return [
            (extents.x - content.x) / self.scale,
            (extents.y - content.y) / self.scale,
            extents.width / self.scale,
            extents.height / self.scale,
        ]

    def rows(self) -> list[dict]:
        """Sidebar rows in display order, as `electron_bridge.mjs` reports them."""
        atspi = _atspi()
        sidebar = self._find("sidebar")
        content = self._content()
        rows = []
        stack = [sidebar]
        while stack:
            node = stack.pop()
            accessible_id = node.get_accessible_id() or ""
            # Rows do not nest: a row's own controls are not rows.
            if node is sidebar or not accessible_id:
                for index in reversed(range(node.get_child_count())):
                    stack.append(node.get_child_at_index(index))
                continue
            node.clear_cache()
            states = node.get_state_set()
            rows.append({
                "key": accessible_id,
                "label": node.get_name() or "",
                "selected": states.contains(atspi.StateType.SELECTED),
                "expanded": states.contains(atspi.StateType.EXPANDED) if states.contains(atspi.StateType.EXPANDABLE) else None,
                "rect": self._rect(node, content),
            })
        rows.sort(key=lambda row: row["rect"][1])
        return rows

    def rects(self, targets: dict[str, str]) -> dict[str, list[float] | None]:
        result = {}
        content = self._content()
        for name, accessible_id in targets.items():
            try:
                result[name] = self._rect(self._find(accessible_id), content)
            except LookupError:
                result[name] = None
        return result

    def fingerprint(self) -> str:
        """What a settled window holds still: its rows and their boxes."""
        return json.dumps(self.rows())

    def busy(self) -> bool:
        """The native client's reads land in rows and markers; no request
        count is exposed, so its fingerprint alone decides."""
        return False

    def targets(self, specs: dict[str, str]) -> dict[str, list[float] | None]:
        """Rectangles of controls by accessible ID."""
        return self.rects(specs)

    # --- input -----------------------------------------------------------

    def wait_for(self, condition: dict, timeout_s: float) -> bool:
        """Whether the condition held within the timeout: ``{"elements": ids}``
        for accessible elements, ``{"marker": name}`` for a diagnostic marker
        emitted since the last input."""
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if "elements" in condition:
                try:
                    for accessible_id in condition["elements"]:
                        self._find(accessible_id)
                    self._read_marks(0.3)
                    return True
                except LookupError:
                    pass
            elif "named" in condition:
                wanted = condition["named"]
                present = any(
                    node.get_name() == wanted["name"]
                    and ("role" not in wanted or node.get_role_name() == wanted["role"])
                    and ("id" not in wanted or node.get_accessible_id() == wanted["id"])
                    for node in self._walk()
                )
                if present == condition.get("present", True):
                    self._read_marks(0.3)
                    return True
            elif any(mark["marker"] == condition["marker"] for mark in self.marks[self._input_mark:]):
                self._read_marks(0.3)
                return True
            self._read_marks(0.1)
        return False

    def click(self, accessible_id: str) -> None:
        """A real pointer click at the target's centre. If synthetic pointer
        input fails, use the target's accessibility action, recorded as a
        substitution so the pointer path is never reported as verified."""
        self._input_mark = len(self.marks)
        atspi = _atspi()
        self._activate()
        x, y = self._screen_point(self._find(accessible_id))
        atspi.generate_mouse_event(x, y, "abs")
        time.sleep(0.05)
        if self._pointer() != (x, y):
            self.substitutions.append({"kind": "pointer", "key": accessible_id, "reference": "click", "replacedBy": "accessibility action"})
            self.activate(accessible_id)
            return
        atspi.generate_mouse_event(x, y, "b1c")

    def _pointer(self) -> tuple[int, int]:
        location = dict(
            line.split("=", 1)
            for line in subprocess.run(["xdotool", "getmouselocation", "--shell"], capture_output=True, text=True, check=True).stdout.split()
        )
        return int(location["X"]), int(location["Y"])

    def activate(self, accessible_id: str) -> None:
        """The target's accessibility action, as a screen reader invokes it."""
        self._input_mark = len(self.marks)
        node = self._find(accessible_id)
        action = node.get_action_iface()
        if action is None or action.get_n_actions() == 0:
            raise LookupError(f"{accessible_id!r} offers no accessibility action")
        action.do_action(0)

    def click_target(self, accessible_id: str) -> None:
        self.click(accessible_id)

    def key(self, name: str) -> None:
        self._input_mark = len(self.marks)
        atspi = _atspi()
        from gi.repository import Gdk

        keyval = Gdk.keyval_from_name(name)
        atspi.generate_keyboard_event(keyval, None, atspi.KeySynthType.SYM)

    def type_text(self, text: str) -> None:
        self._input_mark = len(self.marks)
        atspi = _atspi()
        atspi.generate_keyboard_event(0, text, atspi.KeySynthType.STRING)

    # --- evidence --------------------------------------------------------

    def screenshot(self, path: Path) -> dict:
        """The application's content: the active window's client area, cut to
        the content root inside any client-side frame line."""
        self._activate()
        window = self._window()
        geometry = dict(line.split("=", 1) for line in subprocess.run(
            ["xdotool", "getwindowgeometry", "--shell", window], check=True, capture_output=True, text=True,
        ).stdout.split())
        x11_capture.capture(window, int(geometry["WIDTH"]), int(geometry["HEIGHT"]), path)
        content = self._content()
        image = png.read(path)
        # The capture leaves out the frame's shadow band but keeps its line,
        # which surrounds the content evenly. AccessKit reports no frame
        # extents, so the line's width comes from the capture itself.
        line_x = (image.width - content.width) // 2
        line_y = (image.height - content.height) // 2
        if not (0 <= line_x <= MAX_FRAME_LINE and 0 <= line_y <= MAX_FRAME_LINE):
            raise RuntimeError(
                f"the active window ({image.width}x{image.height}) is not the native client "
                f"({content.width}x{content.height} content)"
            )
        if (line_x, line_y) != (0, 0):
            png.write(path, png.crop(image, line_x, line_y, content.width, content.height))
        return {"path": str(path), "crop": [line_x, line_y, content.width, content.height]}

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
