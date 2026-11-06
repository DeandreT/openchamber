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
import urllib.error
import urllib.request
from pathlib import Path

import ime_fixture
import png
import steps
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

    def paint_regions(self) -> dict:
        """Conservative blank-composer coverage; the full editor stays protected."""
        result = {"regions": [], "protectedBoxes": [], "composerGradientRegions": [],
                  "composerGradientProtected": [], "composerGradientProtectionComplete": False}
        try:
            composer = self._find("composer")
        except LookupError:
            return result
        content = self._content()
        x, y, width, height = self._rect(composer, content)
        protected = []
        has_editor = False
        stack = [(composer, 0)]
        while stack:
            node, depth = stack.pop()
            count = node.get_child_count()
            if depth >= 24 and count:
                return result
            if node is not composer:
                role = node.get_role_name()
                has_editor |= role == "entry"
                if node.get_name() or count == 0 or role not in ("filler", "panel", "section", "group"):
                    box = self._rect(node, content)
                    if box[2] > 0 and box[3] > 0:
                        protected.append(box)
                        continue
                    else:
                        return result
            stack.extend((node.get_child_at_index(index), depth + 1) for index in range(count))
        if not has_editor or width <= height or height <= 4:
            return result
        # The central strip is inside every legal rounded corner. Keep the
        # full text-entry box, including its caret/selection, out of the mask.
        result.update(composerGradientRegions=[[x + height / 2, y + 2, width - height, height / 2 - 2]],
                      composerGradientProtected=protected, composerGradientProtectionComplete=True)
        return result

    # --- input -----------------------------------------------------------

    def wait_for(self, condition: dict, timeout_s: float) -> bool:
        """Whether the condition held within the timeout: ``{"elements": ids}``
        for accessible elements, ``{"named": ...}`` for a node by name,
        ``{"focusedText": text}`` for the focused editable's exact text, or
        ``{"marker": name}`` for a diagnostic marker emitted since the last input."""
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
                    ((node.get_name() or "").startswith(wanted["name"]) if wanted.get("prefix") else node.get_name() == wanted["name"])
                    and ("role" not in wanted or node.get_role_name() == wanted["role"])
                    and ("id" not in wanted or node.get_accessible_id() == wanted["id"])
                    for node in self._walk()
                )
                if present == condition.get("present", True):
                    self._read_marks(0.3)
                    return True
            elif "focusedText" in condition:
                if self.focused_text() == condition["focusedText"]:
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
        self._click_node(self._find(accessible_id), accessible_id)

    def _click_node(self, node, label: str) -> None:
        self._input_mark = len(self.marks)
        atspi = _atspi()
        self._activate()
        x, y = self._screen_point(node)
        atspi.generate_mouse_event(x, y, "abs")
        time.sleep(0.05)
        if self._pointer() != (x, y):
            self.substitutions.append({"kind": "pointer", "key": label, "reference": "click", "replacedBy": "accessibility action"})
            self._do_action(node, label)
            return
        atspi.generate_mouse_event(x, y, "b1c")

    def click_named(self, query: dict) -> None:
        """A pointer click on the one node with this accessible name and one
        of the AT-SPI roles its ARIA role maps to."""
        roles = query.get("roles")
        matches = [node for node in self._walk()
                   if node.get_name() == query["name"] and (roles is None or node.get_role_name() in roles)]
        if len(matches) != 1:
            raise LookupError(f"{len(matches)} native elements are named {query['name']!r} with roles {roles}")
        self._click_node(matches[0], f"named:{query['name']}")

    def hover(self, accessible_id: str) -> None:
        """Moves the real pointer onto the target. Hover has no accessibility
        equivalent, so a pointer the host does not move fails the step."""
        self._input_mark = len(self.marks)
        self._activate()
        x, y = self._screen_point(self._find(accessible_id))
        _atspi().generate_mouse_event(x, y, "abs")
        time.sleep(0.05)
        if self._pointer() != (x, y):
            raise RuntimeError(f"the pointer did not reach {accessible_id}")

    def _pointer(self) -> tuple[int, int]:
        location = dict(
            line.split("=", 1)
            for line in subprocess.run(["xdotool", "getmouselocation", "--shell"], capture_output=True, text=True, check=True).stdout.split()
        )
        return int(location["X"]), int(location["Y"])

    def activate(self, accessible_id: str) -> None:
        """The target's accessibility action, as a screen reader invokes it."""
        self._input_mark = len(self.marks)
        self._do_action(self._find(accessible_id), accessible_id)

    @staticmethod
    def _do_action(node, label: str) -> None:
        action = node.get_action_iface()
        if action is None or action.get_n_actions() == 0:
            raise LookupError(f"{label!r} offers no accessibility action")
        action.do_action(0)

    def click_target(self, accessible_id: str) -> None:
        self.click(accessible_id)

    def key(self, spec: str) -> None:
        """A bare key through AT-SPI's key synthesis, as before; a chord
        through XTest on the owned display, modifiers held around the key."""
        chord = steps.parse_key(spec)
        self._input_mark = len(self.marks)
        if not chord.modifiers:
            atspi = _atspi()
            from gi.repository import Gdk

            keyval = Gdk.keyval_from_name(chord.key)
            atspi.generate_keyboard_event(keyval, None, atspi.KeySynthType.SYM)
            return
        self._activate()
        self._xdotool("key", chord.xdotool())

    def begin_key(self, spec: str):
        """The chord, sent at once: XTest does not wait for the client to
        read the clipboard. Returns a check, already true."""
        self.key(spec)
        return lambda: True

    def type_text(self, text: str) -> None:
        self._input_mark = len(self.marks)
        atspi = _atspi()
        atspi.generate_keyboard_event(0, text, atspi.KeySynthType.STRING)

    @staticmethod
    def _xdotool(*arguments: str) -> None:
        virtual_display.verify_owned_display()
        subprocess.run(["xdotool", *arguments], check=True, capture_output=True, timeout=15)

    def focused_text(self) -> str | None:
        """The text of the focused editable element, or None."""
        atspi = _atspi()
        for node in self._walk():
            if node.get_state_set().contains(atspi.StateType.FOCUSED) and node.get_text_iface() is not None:
                # GI's Accessible.get_text() is the interface getter and shadows Text.get_text.
                return atspi.Text.get_text(node.get_text_iface(), 0, -1)
        return None

    # --- scenario tools --------------------------------------------------
    # The runner attaches the services a scenario needs: `server_url` for
    # reads, `faults` (a RuleFaultProxy), `ibus` (an IBusFixture), `portal`
    # (a running PortalFixture) and `drop_source` (a DropSource).

    server_url: str | None = None
    faults = None
    ibus = None
    portal = None
    drop_source = None

    def compose(self, keys: str, _preedit: str | None = None) -> None:
        """Pinyin keys typed into the private IBus engine, left composing;
        the engine makes its own preedit."""
        if self.ibus is None:
            raise RuntimeError("IME steps need the scenario's IBus fixture")
        self._input_mark = len(self.marks)
        self._activate()
        self.ibus.select(ime_fixture.ENGINE)
        self._xdotool("type", "--clearmodifiers", "--delay", "40", keys)

    def commit_composition(self, mode: str, _text: str) -> None:
        """Space takes the engine's first candidate; Return commits the raw keys."""
        self._input_mark = len(self.marks)
        self._activate()
        self._xdotool("key", "space" if mode == "candidate" else "Return")
        self.ibus.select(ime_fixture.LATIN)

    def drop(self, accessible_id: str, files: list[Path]) -> dict:
        """Files dragged from an XDND source window onto the target."""
        self._input_mark = len(self.marks)
        self._activate()
        return self.drop_source.drop(files, self._screen_point(self._find(accessible_id)))

    def pick(self, files: list[Path], timeout_s: float) -> dict:
        """Chooses the file in the portal's GTK chooser the client opened:
        the location entry takes its path, Return opens it."""
        if self.portal is None:
            raise RuntimeError("pick steps need the scenario's portal fixture")
        deadline = time.monotonic() + timeout_s
        windows: list[str] = []
        pid = None
        while not windows and time.monotonic() < deadline:
            application = self.portal.picker_application()
            if application is not None:
                pid = application.get_process_id()
                windows = subprocess.run(["xdotool", "search", "--onlyvisible", "--pid", str(pid)], capture_output=True, text=True).stdout.split()
            time.sleep(0.1)
        if not windows:
            raise TimeoutError("no file chooser opened")
        self._input_mark = len(self.marks)
        self._xdotool("windowraise", windows[-1])
        self._xdotool("windowfocus", "--sync", windows[-1])
        self._xdotool("key", "ctrl+l")
        time.sleep(0.2)
        self._xdotool("type", "--clearmodifiers", "--delay", "10", str(files[0]))
        time.sleep(0.2)
        self._xdotool("key", "Return")
        while time.monotonic() < deadline:
            if not subprocess.run(["xdotool", "search", "--onlyvisible", "--pid", str(pid)], capture_output=True, text=True).stdout.split():
                return {"window": windows[-1]}
            time.sleep(0.1)
        raise TimeoutError("the file chooser did not close after choosing")

    def api_get(self, path: str) -> dict:
        """``{"status", "body"}`` from the isolated server, never through the fault proxy."""
        if self.server_url is None:
            raise RuntimeError("no server to read")
        request = urllib.request.Request(self.server_url + path)
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                status, text = response.status, response.read().decode()
        except urllib.error.HTTPError as error:
            status, text = error.code, error.read().decode(errors="replace")
        try:
            return {"status": status, "body": json.loads(text)}
        except ValueError:
            return {"status": status, "body": text}

    def add_fault(self, rule: dict) -> None:
        if self.faults is None:
            raise RuntimeError("fault steps need the scenario's fault proxy")
        self.faults.add(rule)

    def release_fault(self, fault_id: str) -> None:
        self.faults.release(fault_id)

    def fault_hits(self) -> list[dict]:
        return self.faults.hit_list() if self.faults is not None else []

    def restart(self, after: str, timeout_s: float) -> dict:
        """Quits with SIGTERM (or waits for an exit the scenario caused) and
        starts again with the same profile and server."""
        if after == "signal":
            try:
                os.kill(self.process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        try:
            self.process.wait(timeout=timeout_s)
            closed = {"exitCode": self.process.returncode}
        except subprocess.TimeoutExpired:
            closed = {"exitCode": None, "signal": "timeout"}
        self.close()
        self._selector.unregister(self.process.stdout)
        self.process.stdout.close()
        self.marks, self._buffer, self._input_mark, self._app = [], "", 0, None
        return {"closed": closed, "launch": self.launch()}

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
