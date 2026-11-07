"""An XDND drag source on the owned display for the native `drop` step.

A small undecorated GTK window offers the files as `text/uri-list`; real
XTest pointer input presses in it and drags onto the native target, so the
drop reaches the client through the X11 drag-and-drop protocol, as a file
manager's would. GTK speaks XDND for the source; the runner pumps its main
loop between pointer moves. Xvfb has no window manager, so the source maps at
the screen origin and the drag starts there.
"""

from __future__ import annotations

import subprocess
import time
from pathlib import Path

import virtual_display

SOURCE_SIZE = 48
# Pointer moves after the press; GTK starts a drag past its threshold.
DRAG_STEPS = 24


class DropSource:
    def __init__(self):
        virtual_display.verify_owned_display()
        import gi

        gi.require_version("Gtk", "4.0")
        gi.require_version("Gdk", "4.0")
        from gi.repository import Gdk, GLib, Gtk

        Gtk.init()
        self.gdk, self.glib, self.gtk = Gdk, GLib, Gtk

    def _pump(self, seconds: float) -> None:
        context = self.glib.MainContext.default()
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            while context.pending():
                context.iteration(False)
            time.sleep(0.01)

    def _pointer(self, *arguments: str) -> None:
        subprocess.run(["xdotool", *arguments], check=True, capture_output=True, timeout=10)
        self._pump(0.03)

    def drop(self, paths: list[Path], target: tuple[int, int], timeout_s: float = 10) -> dict:
        """Drags the files from the source window to the screen point
        `target` and returns how the drag ended."""
        virtual_display.verify_owned_display()
        uris = "".join(f"{path.as_uri()}\r\n" for path in paths).encode()
        provider = self.gdk.ContentProvider.new_for_bytes("text/uri-list", self.glib.Bytes.new(uris))
        outcome = {"began": False, "ended": False, "deleted": False}
        window = self.gtk.Window(decorated=False, default_width=SOURCE_SIZE, default_height=SOURCE_SIZE, title="parity drop source")
        area = self.gtk.Box(hexpand=True, vexpand=True)
        source = self.gtk.DragSource(actions=self.gdk.DragAction.COPY)
        source.connect("prepare", lambda *_: provider)
        source.connect("drag-begin", lambda *_: outcome.update(began=True))
        source.connect("drag-end", lambda _source, _drag, deleted: outcome.update(ended=True, deleted=deleted))
        area.add_controller(source)
        window.set_child(area)
        window.present()
        try:
            deadline = time.monotonic() + timeout_s
            while not window.get_mapped() and time.monotonic() < deadline:
                self._pump(0.05)
            self._pump(0.2)
            surface = window.get_surface()
            xid = str(surface.get_xid())
            subprocess.run(["xdotool", "windowraise", xid], check=True, capture_output=True, timeout=10)
            self._pump(0.1)
            start = (SOURCE_SIZE // 2, SOURCE_SIZE // 2)
            self._pointer("mousemove", str(start[0]), str(start[1]))
            self._pointer("mousedown", "1")
            for step in range(1, DRAG_STEPS + 1):
                x = start[0] + (target[0] - start[0]) * step // DRAG_STEPS
                y = start[1] + (target[1] - start[1]) * step // DRAG_STEPS
                self._pointer("mousemove", str(x), str(y))
            # The target answers XdndPosition before the release.
            self._pump(0.4)
            self._pointer("mouseup", "1")
            while not outcome["ended"] and time.monotonic() < deadline:
                self._pump(0.05)
        finally:
            window.destroy()
            self._pump(0.1)
        if not outcome["began"]:
            raise RuntimeError("the drag never started from the source window")
        if not outcome["ended"]:
            raise TimeoutError("the drop did not finish")
        return {"files": [path.name for path in paths], "target": list(target), **outcome}
