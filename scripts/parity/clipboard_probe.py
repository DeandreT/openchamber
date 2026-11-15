"""Read the result of a real copy action on the owned test display."""

import secrets

import virtual_display


class ClipboardProbe:
    """Initialize before launching the client; GTK initialization can change focus."""

    def __init__(self):
        virtual_display.verify_owned_display()
        import gi

        gi.require_version("Gtk", "4.0")
        gi.require_version("Gdk", "4.0")
        from gi.repository import Gdk, Gio, GLib, Gtk

        Gtk.init()
        self.glib = GLib
        self.gio = Gio
        self.display = Gdk.Display.get_default()
        self.clipboard = self.display.get_clipboard()

    def paste(self, text, dispatch, observed, timeout_ms=5000):
        """Serve exact Unicode to a real paste and wait for its visible result."""
        virtual_display.verify_owned_display()
        if observed():
            raise ValueError("paste result is already present before dispatch")
        loop = self.glib.MainLoop()
        finished = False
        timed_out = False

        def check():
            nonlocal finished
            try:
                finished = observed()
            except (LookupError, RuntimeError):
                return True
            if finished:
                loop.quit()
                return False
            return True

        def expire():
            nonlocal timed_out
            timed_out = True
            loop.quit()
            return False

        poll = self.glib.timeout_add(20, check)
        timer = self.glib.timeout_add(timeout_ms, expire)
        try:
            self.clipboard.set(text)
            self.display.sync()
            dispatch()
            loop.run()
        finally:
            if not finished:
                self.glib.source_remove(poll)
            if not timed_out:
                self.glib.source_remove(timer)
        if not finished:
            raise TimeoutError("paste did not produce the expected text")

    def copy(self, dispatch, timeout_ms=5000) -> str:
        """Dispatch once and wait for a new clipboard owner and its exact text."""
        virtual_display.verify_owned_display()
        marker = f"openchamber-copy-probe-{secrets.token_hex(16)}"
        loop = self.glib.MainLoop()
        result = []
        failure = []
        pending = False
        timed_out = False
        cancellable = self.gio.Cancellable()

        def received(clipboard, operation):
            nonlocal pending
            pending = False
            try:
                text = clipboard.read_text_finish(operation)
            except self.glib.Error as error:
                if cancellable.is_cancelled():
                    return
                failure.append(str(error))
                loop.quit()
                return
            if text != marker:
                if text is None:
                    failure.append("copy produced no text")
                else:
                    result.append(text)
                loop.quit()

        def changed(*_):
            nonlocal pending
            if not pending and not self.clipboard.is_local():
                pending = True
                self.clipboard.read_text_async(cancellable, received)

        def expired():
            nonlocal timed_out
            timed_out = True
            failure.append("copy did not produce new clipboard text before the deadline")
            loop.quit()
            return False

        handler = self.clipboard.connect("changed", changed)
        timer = self.glib.timeout_add(timeout_ms, expired)
        try:
            self.clipboard.set(marker)
            self.display.sync()
            context = self.glib.MainContext.default()
            while context.pending():
                context.iteration(False)
            dispatch()
            changed()
            if not result and not failure:
                loop.run()
        finally:
            self.clipboard.disconnect(handler)
            cancellable.cancel()
            if not timed_out:
                self.glib.source_remove(timer)
        if failure:
            raise TimeoutError(failure[0])
        return result[0]
