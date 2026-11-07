"""Electron side of the parity runner, through `electron_bridge.mjs`."""

from __future__ import annotations

import json
import os
import select
import subprocess
import time
from pathlib import Path

import steps

BRIDGE = Path(__file__).with_name("electron_bridge.mjs")


FINGERPRINT = """(() => JSON.stringify([
  document.body.innerText.length,
  ...[...document.querySelectorAll('aside, section, nav [role=option] > span, [data-sidebar-row-key], [data-message-id]')].slice(0, 400).map((element) => {
    const rect = element.getBoundingClientRect()
    return [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)]
  }),
]))()"""


# The focused editable's text: a field's value, or a contenteditable's text
# without the trailing line break an editor keeps.
FOCUSED_TEXT = """(() => {
  let element = document.activeElement
  while (element?.shadowRoot?.activeElement) element = element.shadowRoot.activeElement
  if (!element) return null
  if (typeof element.value === 'string') return element.value
  return element.isContentEditable ? element.innerText.replace(/\\n$/, '') : null
})()"""


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
        self._buffer = b""
        self._replies: dict[int, dict] = {}

    def _send(self, command: str, **params) -> int:
        if self._bridge is None or self._bridge.stdin is None or self._bridge.stdout is None:
            raise RuntimeError("the Electron bridge is not running")
        if {"id", "cmd"} & params.keys():
            raise ValueError(f"{command}: id and cmd address the bridge message, not a parameter")
        self._next_id += 1
        self._bridge.stdin.write(json.dumps({"id": self._next_id, "cmd": command, **params}) + "\n")
        self._bridge.stdin.flush()
        return self._next_id

    def _reply(self, request: int, command: str, wait: bool = True) -> dict | None:
        """The bridge answers in order, one line per command. Without
        `wait`, None while the answer has not arrived."""
        while request not in self._replies:
            if b"\n" in self._buffer:
                line, self._buffer = self._buffer.split(b"\n", 1)
                reply = json.loads(line)
                self._replies[reply["id"]] = reply
                continue
            if not wait and not select.select([self._bridge.stdout], [], [], 0)[0]:
                return None
            chunk = os.read(self._bridge.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError(f"the Electron bridge exited during {command}")
            self._buffer += chunk
        reply = self._replies.pop(request)
        if not reply.pop("ok", False):
            raise RuntimeError(f"{command}: {reply.get('error')}")
        reply.pop("id")
        return reply

    def _call(self, command: str, **params) -> dict:
        return self._reply(self._send(command, **params), command)

    def begin_key(self, spec: str):
        """Sends a chord without waiting: Chromium answers a Ctrl+V only after
        it read the clipboard, which the caller's own loop may have to serve.
        Returns a check that is true once the chord was handled."""
        request = self._send("key", events=steps.dom_key_events(steps.parse_key(spec)))
        return lambda: self._reply(request, "key", wait=False) is not None

    def launch(self) -> dict:
        import virtual_display
        env = virtual_display.child_environment(self.env)
        self._bridge = subprocess.Popen(
            ["node", str(BRIDGE)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        log_path = Path(env["OPENCHAMBER_DATA_DIR"]).parent / "electron.log"
        return self._call("launch", app=str(self.app), env=env, scale=self.scale, width=self.width, height=self.height, logPath=str(log_path))

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

    def fingerprint(self) -> str:
        """What a settled page holds still: its text and the boxes of its
        panels, sections, prompt ticks, rows and messages."""
        return self._call("observe", expression=FINGERPRINT)["value"]

    def busy(self) -> bool:
        """Whether the page still has fetch or XHR requests in flight: a
        panel whose data has not arrived can hold still for a while."""
        return self._call("inflight")["count"] > 0

    def pending_requests(self) -> list[str]:
        """Route prefixes only; query strings and request content stay private."""
        return self._call("inflight")["routes"]

    def wait_for(self, condition: dict, timeout_s: float) -> bool:
        """Whether the page expression ``{"expression": ...}``, or the focused
        editable's exact text ``{"focusedText": ...}``, held within the
        timeout, followed by a painted frame."""
        if "focusedText" in condition:
            condition = {"expression": f"({FOCUSED_TEXT}) === {json.dumps(condition['focusedText'])}"}
        try:
            self._call("wait", expression=condition["expression"], timeoutMs=int(timeout_s * 1000))
        except RuntimeError:
            return False
        return True

    def rects(self, targets: dict[str, str]) -> dict[str, list[float] | None]:
        selectors = {name: f'[data-sidebar-row-key="{key}"]' for name, key in targets.items()}
        return self._call("rects", selectors=selectors)["rects"]

    def targets(self, specs: dict[str, dict]) -> dict[str, list[float] | None]:
        """Rectangles of controls by ``{"selector": ...}`` or ``{"button": text}``."""
        return self._call("targets", specs=specs)["rects"]

    def text_boxes(self) -> list[list[float]]:
        """The boxes of the visible text, ``[x, y, width, height]`` in CSS
        pixels."""
        return self._call("textBoxes")["boxes"]

    def paint_regions(self) -> dict:
        return self._call("paintRegions")

    def click(self, row_key: str) -> None:
        row = f'[data-sidebar-row-key="{row_key}"]'
        self._call("click", selector=f"{row} [data-session-row], {row} button", within=row)

    def click_target(self, spec: dict) -> None:
        self._call("click", **spec)

    def key(self, spec: str) -> None:
        """A chord such as ``"ctrl+shift+z"``, with its modifiers pressed and
        released around the key."""
        self._call("key", events=steps.dom_key_events(steps.parse_key(spec)))

    def type_text(self, text: str) -> None:
        self._call("type", text=text)

    def hover(self, spec: dict) -> None:
        self._call("hover", **spec)

    def click_named(self, query: dict) -> None:
        """A pointer click on the one node with this accessible name and
        ARIA role, found in Chromium's accessibility tree."""
        self._call("clickNamed", name=query["name"], role=query.get("role"))

    def focused_text(self) -> str | None:
        return self._call("observe", expression=FOCUSED_TEXT)["value"]

    def compose(self, keys: str, preedit: str | None = None) -> None:
        """IME composition through CDP: each key extends the marked text,
        which ends as the engine's `preedit` when one is given."""
        self._call("ime", compose=keys, preedit=preedit)

    def commit_composition(self, _mode: str, text: str) -> None:
        """Commits `text`: the candidate, or the raw keys the runner composed."""
        self._call("ime", commit=text)

    def drop(self, spec: dict, files: list[Path]) -> dict:
        return self._call("drop", **spec, files=[str(path) for path in files])

    def intercept_file_choosers(self) -> None:
        self._call("interceptFileChoosers")

    def pick(self, files: list[Path], timeout_s: float) -> dict:
        """Answers the file chooser the page opened, once it has opened."""
        return self._call("pick", files=[str(path) for path in files], timeoutMs=int(timeout_s * 1000))

    def api_get(self, path: str) -> dict:
        """``{"status", "body"}`` from the in-process server."""
        return self._call("api", path=path)

    def add_fault(self, rule: dict) -> None:
        self._call("fault", rule=rule)

    def release_fault(self, fault_id: str) -> None:
        self._call("release", faultId=fault_id)

    def fault_hits(self) -> list[dict]:
        return self._call("faults")["hits"]

    def restart(self, after: str, timeout_s: float) -> dict:
        """Quits (SIGTERM, or waits for an exit the scenario caused) and
        starts again on the same profile; the runner then waits for readiness."""
        return self._call("restart", after=after, timeoutMs=int(timeout_s * 1000))

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
