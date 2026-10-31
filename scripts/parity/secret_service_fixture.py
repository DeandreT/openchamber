"""A private KDE Secret Service for native credential-store qualification."""

import os
from pathlib import Path
import subprocess

from native_adapter import _atspi
from native_interactions import until
import virtual_display


class SecretServiceFixture:
    """Own an encrypted wallet under the test root and never use desktop services."""

    def __init__(self, root: Path):
        self.root = root
        self.process = None
        self.seed = None
        self.log = None

    def __enter__(self):
        virtual_display.verify_owned_display()
        self.root.mkdir(mode=0o700)
        self.env = {key: os.environ[key] for key in (
            "PATH", "DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR",
        ) if key in os.environ}
        self.env.update(HOME=str(self.root), XDG_CONFIG_HOME=str(self.root / "config"),
                        XDG_DATA_HOME=str(self.root / "data"), XDG_CACHE_HOME=str(self.root / "cache"),
                        QT_QPA_PLATFORM="xcb", LANG="C.UTF-8", KCRASH_AUTO_RESTART="0",
                        QT_LINUX_ACCESSIBILITY_ALWAYS_ON="1")
        for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
            Path(self.env[key]).mkdir(mode=0o700)
        (self.root / "config/kwalletrc").write_text(
            "[Wallet]\nEnabled=true\nFirst Use=false\nPrompt on Open=false\n[org.freedesktop.secrets]\napiEnabled=true\n"
        )
        self.log = (self.root / "service.log").open("w")
        try:
            self.process = subprocess.Popen(["ksecretd"], env=self.env, stdout=self.log, stderr=self.log)
            until("private Secret Service did not start", lambda: subprocess.run(
                ["busctl", "--user", "status", "org.freedesktop.secrets"], env=self.env,
                capture_output=True, timeout=2,
            ).returncode == 0, 15)
            self.seed = subprocess.Popen(["secret-tool", "store", "--label=Native qualification", "application", "native-qualification"],
                                         env=self.env, stdin=subprocess.PIPE, stdout=self.log, stderr=self.log)
            self.seed.stdin.write(b"synthetic-qualification-value\n")
            self.seed.stdin.close()
            self.click("Classic, blowfish encrypted file")
            self.click("Finish")
            inputs = until("wallet password fields did not appear", lambda: self.password_inputs(), 15)
            if len(inputs) != 2:
                raise RuntimeError("unexpected private wallet password form")
            for node in inputs:
                self.click_node(node)
                subprocess.run(["xdotool", "type", "--clearmodifiers", "Private-fixture-only-593!"], env=self.env, check=True)
            self.click("OK")
            if self.seed.wait(timeout=15) != 0:
                raise RuntimeError("private Secret Service could not store an item")
            subprocess.run(["secret-tool", "clear", "application", "native-qualification"],
                           env=self.env, check=True, stdout=self.log, stderr=self.log, timeout=10)
            return self
        except BaseException:
            self.close()
            raise

    def nodes(self):
        desktop = _atspi().get_desktop(0)
        stack = [desktop.get_child_at_index(index) for index in range(desktop.get_child_count())]
        while stack:
            node = stack.pop()
            if node.get_role_name() == "application" and node.get_process_id() != self.process.pid:
                continue
            yield node
            stack.extend(node.get_child_at_index(index) for index in range(node.get_child_count()))

    def password_inputs(self):
        return [node for node in self.nodes() if node.get_role_name() == "password text"
                and node.get_state_set().contains(_atspi().StateType.SHOWING)]

    def click(self, name):
        node = until(f"private wallet control did not appear: {name}", lambda: next((
            node for node in self.nodes() if node.get_name() == name
            and node.get_state_set().contains(_atspi().StateType.SHOWING)
        ), None), 15)
        self.click_node(node)

    def click_node(self, node):
        bounds = node.get_extents(_atspi().CoordType.SCREEN)
        if bounds.width <= 0 or bounds.height <= 0:
            raise RuntimeError("private wallet control has no visible bounds")
        subprocess.run(["xdotool", "mousemove", str(bounds.x + bounds.width // 2),
                        str(bounds.y + bounds.height // 2), "click", "1"], env=self.env, check=True)

    def close(self):
        for process in (self.seed, self.process):
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        if self.log is not None:
            self.log.close()

    def __exit__(self, *_):
        self.close()
