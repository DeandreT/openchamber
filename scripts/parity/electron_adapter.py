"""Electron side of the parity runner, through `electron_bridge.mjs`."""

from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path

BRIDGE = Path(__file__).with_name("electron_bridge.mjs")


class ElectronClient:
    """One packaged Electron process under test, driven over CDP."""

    def __init__(self, app: Path, env: dict[str, str], scale: float, width: int, height: int):
        self.app = app
        self.env = env
        self.scale = scale
        self.width = width
        self.height = height
        self._bridge: subprocess.Popen | None = None
        self._next_id = 0

    def _call(self, command: str, **params) -> dict:
        if self._bridge is None or self._bridge.stdin is None or self._bridge.stdout is None:
            raise RuntimeError("the Electron bridge is not running")
        self._next_id += 1
        self._bridge.stdin.write(json.dumps({"id": self._next_id, "cmd": command, **params}) + "\n")
        self._bridge.stdin.flush()
        line = self._bridge.stdout.readline()
        if not line:
            raise RuntimeError(f"the Electron bridge exited during {command}")
        reply = json.loads(line)
        if not reply.get("ok"):
            raise RuntimeError(f"{command}: {reply.get('error')}")
        return reply

    def launch(self) -> dict:
        self._bridge = subprocess.Popen(
            ["node", str(BRIDGE)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        env = dict(self.env)
        env.pop("WAYLAND_DISPLAY", None)
        env["XDG_SESSION_TYPE"] = "x11"
        return self._call("launch", app=str(self.app), env=env, scale=self.scale, width=self.width, height=self.height)

    def wait_ready(self, timeout_s: float, connected: bool) -> None:
        """Rows and composer are present and the row list held still for 750 ms."""
        self._call(
            "wait",
            expression="document.querySelectorAll('[data-sidebar-row-key]').length > 0 && !!document.querySelector('[data-chat-input=\"true\"]')",
            timeoutMs=int(timeout_s * 1000),
        )
        deadline = time.monotonic() + timeout_s
        previous = None
        stable_since = time.monotonic()
        while time.monotonic() < deadline:
            keys = [row["key"] for row in self.rows()]
            if keys != previous:
                previous = keys
                stable_since = time.monotonic()
            elif time.monotonic() - stable_since >= 0.75:
                return
            time.sleep(0.1)
        raise TimeoutError("the Electron sidebar never settled")

    def rows(self) -> list[dict]:
        return self._call("rows")["rows"]

    def rects(self, targets: dict[str, str]) -> dict[str, list[float] | None]:
        selectors = {name: f'[data-sidebar-row-key="{key}"]' for name, key in targets.items()}
        return self._call("rects", selectors=selectors)["rects"]

    def targets(self, specs: dict[str, dict]) -> dict[str, list[float] | None]:
        """Rectangles of controls by ``{"selector": ...}`` or ``{"button": text}``."""
        return self._call("targets", specs=specs)["rects"]

    def click(self, row_key: str) -> None:
        self._call("click", selector=f'[data-sidebar-row-key="{row_key}"] [data-session-row], [data-sidebar-row-key="{row_key}"] button')

    def key(self, name: str) -> None:
        codes = {"Return": ("Enter", "Enter", 13), "Escape": ("Escape", "Escape", 27), "Down": ("ArrowDown", "ArrowDown", 40), "Up": ("ArrowUp", "ArrowUp", 38)}
        key, code, key_code = codes[name]
        self._call("key", key=key, code=code, keyCode=key_code)

    def type_text(self, text: str) -> None:
        self._call("type", text=text)

    def screenshot(self, path: Path) -> dict:
        return self._call("screenshot", path=str(path))

    def close(self) -> dict:
        if self._bridge is None:
            return {}
        try:
            self._call("close")
        except RuntimeError:
            pass
        if self._bridge.stdin is not None:
            self._bridge.stdin.close()
        try:
            self._bridge.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self._bridge.kill()
        return {}
