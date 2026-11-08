#!/usr/bin/env python3
"""Runs UI parity scenarios against the Electron reference and the native desktop.

    python3 scripts/parity/runner.py seed --root tmp/parity
    python3 scripts/parity/runner.py run --root tmp/parity \\
        --electron-app packages/electron/dist/linux-unpacked/openchamber

`seed` builds the synthetic seed: an isolated server's state with fixed
sessions created through the fixture provider. `run` gives each client its
own copy of that seed, launches it at the standard client size, scale, theme
and locale, performs each scenario's steps through real input, and records
screenshots, sidebar rows and geometry at every checkpoint. The Electron
capture is the reference; the comparison reports every difference and
passes only when there is none. See scripts/parity/DOCUMENTATION.md.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import compare  # noqa: E402
from electron_adapter import ElectronClient  # noqa: E402
import native_adapter  # noqa: E402
import steps as scenario_steps  # noqa: E402
from native_adapter import NativeClient  # noqa: E402
import virtual_display  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
SCENARIOS = Path(__file__).with_name("scenarios.json")
EXCEPTIONS = Path(__file__).with_name("exceptions.json")
INVENTORY = Path(__file__).with_name("inventory.json")
ISOLATED_SERVER = REPO_ROOT / "scripts" / "perf" / "isolated-server.mjs"
FIXTURE_PROVIDER = REPO_ROOT / "scripts" / "perf" / "fixture-provider.mjs"
THEMES = REPO_ROOT / "native" / "crates" / "desktop" / "resources" / "themes.json"
SEED_SESSION = REPO_ROOT / "scripts" / "perf" / "seed-long-session.mjs"
DEFAULT_ELECTRON = REPO_ROOT / "packages" / "electron" / "dist" / "linux-unpacked" / "openchamber"
DEFAULT_NATIVE = REPO_ROOT / "native" / "target" / "release" / "openchamber-native"
DEFAULT_DIST = REPO_ROOT / "packages" / "electron" / "resources" / "web-dist"
DEFAULT_OPENCODE = REPO_ROOT / "packages" / "electron" / "dist" / "linux-unpacked" / "resources" / "opencode-cli" / "opencode"

# The seed: titles, turns and fixture models, created in this order.
SEED_SESSIONS = [
    ("parity: short a", 2, "perf/stream-20000cps"),
    ("parity: short b", 3, "perf/stream-20000cps"),
    ("parity: long", 12, "perf/stream-20000cps,perf/code-20000cps,perf/unicode-20000cps"),
    ("parity: tools", 1, "perf/agent-20tools-300cps"),
]


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class IsolatedServer:
    """`scripts/perf/isolated-server.mjs` on a root this run owns."""

    def __init__(self, root: Path, args: argparse.Namespace, provider: str | None = None):
        self.root = root
        self.port = free_port()
        self.fixture_port = free_port()
        command = [
            "node", str(ISOLATED_SERVER), "--root", str(root),
            "--port", str(self.port), "--fixture-port", str(self.fixture_port),
            "--dist", str(args.dist), "--opencode-binary", str(args.opencode_binary),
            *(["--provider-mode", provider] if provider else []),
        ]
        self.log = open(root / "isolated-server.log", "w")
        self.process = subprocess.Popen(command, cwd=REPO_ROOT, stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def wait_ready(self, timeout_s: float) -> None:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"the isolated server exited; see {self.root / 'isolated-server.log'}")
            try:
                with urllib.request.urlopen(f"{self.url}/health", timeout=2) as response:
                    if json.load(response).get("isOpenCodeReady"):
                        return
            except OSError:
                pass
            time.sleep(0.25)
        raise TimeoutError("the isolated server never became ready")

    def stop(self) -> None:
        # Ctrl-C semantics: the script stops exactly what it started.
        try:
            os.killpg(self.process.pid, 2)
            self.process.wait(timeout=30)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            os.killpg(self.process.pid, 9)
            self.process.wait(timeout=10)
        self.log.close()


class FixtureProvider:
    """The fixture model provider for a client that runs its own server, with
    the OpenCode configuration the isolated server gives its OpenCode. A
    `provider` mode (`steps.PROVIDERS`) is the one `--provider-mode` gives the
    isolated server."""

    def __init__(self, root: Path, provider: str | None = None):
        self.port = free_port()
        self.log = open(root / "fixture.log", "w")
        env = {key: os.environ[key] for key in ("PATH", "LANG") if key in os.environ}
        self.process = subprocess.Popen(
            ["node", str(FIXTURE_PROVIDER), str(self.port), *([f"--{provider}"] if provider else [])],
            cwd=REPO_ROOT, env=env, stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True,
        )
        options = json.dumps({provider: True} if provider else {})
        script = (
            f"import {{ isolatedOpencodeConfig }} from {json.dumps(ISOLATED_SERVER.as_uri())};"
            f"console.log(JSON.stringify(isolatedOpencodeConfig({self.port}, {options})))"
        )
        self.opencode_config = subprocess.run(
            ["node", "--input-type=module", "-e", script], cwd=REPO_ROOT, env=env,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def wait_ready(self, timeout_s: float) -> None:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"the fixture provider exited; see {self.log.name}")
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=1):
                    return
            except OSError:
                time.sleep(0.2)
        raise TimeoutError("the fixture provider did not start")

    def stop(self) -> None:
        try:
            os.killpg(self.process.pid, 15)
            self.process.wait(timeout=10)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            os.killpg(self.process.pid, 9)
            self.process.wait(timeout=10)
        self.log.close()


def seed(args: argparse.Namespace) -> int:
    root = args.root.resolve()
    if (root / "seed").exists() and not args.force:
        print(f"{root / 'seed'} exists; pass --force to rebuild it", file=sys.stderr)
        return 2
    shutil.rmtree(root / "state", ignore_errors=True)
    root.mkdir(parents=True, exist_ok=True)
    server = IsolatedServer(root, args)
    try:
        server.wait_ready(args.timeout)
        for title, turns, models in SEED_SESSIONS:
            subprocess.run(
                ["node", str(SEED_SESSION), "--port", str(server.port), "--dir", str(root / "project"),
                 "--turns", str(turns), "--title", title, "--models", models],
                cwd=REPO_ROOT, check=True,
            )
    finally:
        server.stop()
    shutil.rmtree(root / "seed", ignore_errors=True)
    shutil.copytree(root / "state", root / "seed", symlinks=True)
    print(f"seed: {root / 'seed'}")
    return 0


def copy_seed(root: Path, target: Path, theme: str, settings: dict | None = None, model: str | None = None) -> Path:
    """An independent copy of the seed with the theme pinned. The web reads
    `useSystemTheme: false` together with the derived `themeVariant`, so the
    variant and that variant's theme are pinned too. A scenario's `model` is
    the profile's `defaultModel` preference, which new sessions start on."""
    state = target / "state"
    shutil.copytree(root / "seed", state, symlinks=True)
    preferences = state / "oc-data" / "preferences.json"
    document = json.loads(preferences.read_text()) if preferences.exists() else {"version": 1, "fields": {}}
    now = int(time.time() * 1000)
    for key, value in {**(settings or {}), **({"defaultModel": model} if model else {})}.items():
        document["fields"][key] = {"value": value, "updatedAt": now}
    variant = theme_variant(theme)
    pinned = (("useSystemTheme", False), ("themeId", theme), ("themeVariant", variant), (f"{variant}ThemeId", theme))
    for key, value in pinned:
        document["fields"][key] = {"value": value, "updatedAt": now}
    preferences.write_text(json.dumps(document, indent=2))
    # The login-shell probe runs zsh -il; an empty rc keeps it non-interactive.
    (state / "home").mkdir(parents=True, exist_ok=True)
    (state / "home" / ".zshrc").write_text("")
    return state


def fixture_project(root: Path, kind: str) -> Path:
    """The scenario fixture's project path. It is the same for both clients,
    so project IDs and row keys match; each client gets a fresh copy there
    before it starts, and runs are serial."""
    return root / "fixtures" / kind / "fixture-repo"


def prepare_fixture(root: Path, kind: str, state: Path) -> Path:
    """Recreates the fixture project and registers it as the copy's active
    project. `git` is `isolated-server.mjs --git-fixture`'s repository, with
    its bare remote beside it."""
    project = fixture_project(root, kind)
    shutil.rmtree(project.parent, ignore_errors=True)
    project.parent.mkdir(parents=True)
    # The isolated server's own order for --git-fixture.
    script = (
        f"import {{ ensureGitFixture, ensureProject }} from {json.dumps(ISOLATED_SERVER.as_uri())};"
        f"ensureProject({json.dumps(str(project))});"
        f"ensureGitFixture({json.dumps(str(project.parent))}, {json.dumps(str(project))})"
    )
    project.mkdir()
    subprocess.run(["node", "--input-type=module", "-e", script], cwd=REPO_ROOT, check=True, capture_output=True,
                   env={key: os.environ[key] for key in ("PATH", "LANG") if key in os.environ})
    server_settings = state / "oc-data" / "settings.json"
    document = json.loads(server_settings.read_text())
    identifier = "path_" + base64.urlsafe_b64encode(str(project).encode()).decode().rstrip("=")
    now = int(time.time() * 1000)
    document["projects"] = [entry for entry in document.get("projects", []) if entry.get("path") != str(project)]
    document["projects"].append({"id": identifier, "path": str(project), "label": project.name,
                                 "addedAt": now, "lastOpenedAt": now, "sidebarCollapsed": False})
    document["activeProjectId"] = identifier
    document["lastDirectory"] = str(project)
    server_settings.write_text(json.dumps(document, indent=2))
    return project


def theme_variant(theme: str) -> str:
    """`light` or `dark`, from the exported theme catalog."""
    for entry in json.loads(THEMES.read_text())["themes"]:
        if entry["id"] == theme:
            return entry["variant"]
    raise LookupError(f"no theme {theme} in {THEMES}")


def state_dirs(state: Path) -> dict[str, str]:
    """The home and XDG directories of a client's state copy. Both clients
    get them, so lookups under them (icon themes, default applications) see
    the same files."""
    return {
        "HOME": str(state / "home"),
        "XDG_CONFIG_HOME": str(state / "config"),
        "XDG_DATA_HOME": str(state / "data"),
        "XDG_CACHE_HOME": str(state / "cache"),
        "XDG_STATE_HOME": str(state / "xdg-state"),
    }


CLIENT_ENV_KEYS = ("PATH", "DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR", "LANG", "USER",
                   "LD_LIBRARY_PATH", "VK_ICD_FILENAMES", "LIBGL_DRIVERS_PATH", "MESA_LOADER_DRIVER_OVERRIDE")


def electron_env(state: Path, opencode_config: str) -> dict[str, str]:
    env = {key: os.environ[key] for key in CLIENT_ENV_KEYS if key in os.environ}
    env.update(state_dirs(state))
    env.update(
        SHELL="/bin/zsh",
        LANG="en_US.UTF-8",
        # GTK's automatic IBus module selection can deadlock a private launch.
        # Native IME qualification supplies its own explicit IBus environment.
        GTK_IM_MODULE="gtk-im-context-simple",
        OPENCHAMBER_DATA_DIR=str(state / "oc-data"),
        OPENCHAMBER_DESKTOP_USER_DATA_DIR=str(state / "electron-user-data"),
        OPENCODE_CONFIG_CONTENT=opencode_config,
    )
    return env


def native_env(state: Path, server_url: str, theme: str) -> dict[str, str]:
    env = {key: os.environ[key] for key in CLIENT_ENV_KEYS if key in os.environ}
    env.update(state_dirs(state))
    env.update(
        OPENCHAMBER_NATIVE_SERVER=server_url,
        OPENCHAMBER_NATIVE_THEME=theme,
        OPENCHAMBER_NATIVE_LOCALE="en",
        OPENCHAMBER_NATIVE_LOG="warn",
        LANG="en_US.UTF-8",
    )
    return env


def resolve_row(rows: list[dict], target: dict) -> str:
    """A row key from a symbolic target: a zone or project prefix and a title."""
    prefix = target.get("zone", "")
    matches = [row["key"] for row in rows if row["key"].startswith(prefix) and row["label"] == target["title"]]
    if len(matches) != 1:
        raise LookupError(f"{len(matches)} rows match {target}")
    return matches[0]


# A capture waits until the client's content held still this long.
SETTLE_QUIET_S = 0.75
SETTLE_TIMEOUT_S = 15.0


def settle(client) -> bool:
    """Whether the client's fingerprint held still for the quiet period,
    with no request in flight, before the timeout. Content still loading (a
    panel filling in after the transcript) would otherwise be captured
    midway."""
    deadline = time.monotonic() + SETTLE_TIMEOUT_S
    previous = None
    since = time.monotonic()
    while time.monotonic() < deadline:
        current = client.fingerprint()
        if current != previous or client.busy():
            previous = current
            since = time.monotonic()
        elif time.monotonic() - since >= SETTLE_QUIET_S:
            return True
        time.sleep(0.1)
    return False


def wait_selected(client, key: str, timeout_s: float = 15.0) -> bool:
    """Waits for the requested row, so an unrelated selected session cannot
    satisfy a click's postcondition."""
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if any(row["key"] == key and row.get("selected") for row in client.rows()):
            return True
        time.sleep(0.1)
    return False


class StepContext:
    """What steps beyond pointer and keyboard input need for one client:
    path placeholders, the private clipboard, where step input files go, and
    the last composition."""

    def __init__(self, places: dict[str, str] | None = None, clipboard=None, inputs: Path | None = None,
                 fixture_port: int | None = None):
        self.places = places or {}
        self.clipboard = clipboard
        self.inputs = inputs
        # The fixture provider this client's server uses, for its turn gate.
        self.fixture_port = fixture_port
        self.composed: str | None = None
        self.restarts: list[dict] = []
        self.waits: list[dict] = []
        self._files = 0

    def write_files(self, files: list[dict]) -> list[Path]:
        """Each step's synthetic files in a new directory of their own."""
        self._files += 1
        folder = self.inputs / str(self._files)
        folder.mkdir(parents=True)
        paths = []
        for entry in files:
            path = folder / entry["name"]
            path.write_text(entry["text"], encoding="utf-8", newline="")
            paths.append(path)
        return paths


def require_settled(client, before: str) -> None:
    if not settle(client):
        pending = getattr(client, "pending_requests", lambda: [])()
        raise TimeoutError(f"the client never settled before {before}; pending route prefixes: {pending}")


def resolve_sessions(client, path: str, quote: bool = True) -> str:
    """`{session:<title>}` -> the ID of the one session with that title,
    quoted for a URL path unless `quote` is false (a `select` key)."""
    titles = scenario_steps.SESSION_PLACEHOLDER.findall(path)
    if not titles:
        return path
    listing = client.api_get("/api/session?limit=200")
    sessions = listing["body"].get("data", []) if isinstance(listing["body"], dict) else []
    for title in titles:
        matches = [entry["id"] for entry in sessions if entry.get("title") == title]
        if len(matches) != 1:
            raise LookupError(f"{len(matches)} sessions are titled {title!r}")
        path = path.replace(f"{{session:{title}}}", urllib.parse.quote(matches[0], safe="") if quote else matches[0])
    return path


def turn_gate(context: StepContext, held: bool | None = None) -> dict:
    """The fixture provider's turn gate: while held, a delayed model's turn
    waits for the release instead of its delay. Without `held`, a read."""
    if context.fixture_port is None:
        raise RuntimeError("no fixture provider to gate")
    body = None if held is None else json.dumps({"held": held}).encode()
    request = urllib.request.Request(f"http://127.0.0.1:{context.fixture_port}/__fixture/turn-gate", data=body,
                                     method="GET" if held is None else "PUT", headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=5) as response:
        state = json.load(response)
    if not isinstance(state, dict) or not isinstance(state.get("held"), bool) or type(state.get("waiting")) is not int:
        raise RuntimeError("the fixture returned a malformed turn gate")
    if held is not None and state["held"] != held:
        raise RuntimeError(f"the fixture turn gate did not become held={held}")
    return state


def observe(client, spec: dict, context: StepContext) -> tuple[bool, object]:
    """Reads the expectation's authoritative source once: whether it holds,
    and what was observed. A failed read never holds."""
    if "api" in spec:
        response = client.api_get(resolve_sessions(client, spec["api"]))
        if not 200 <= response["status"] < 300:
            return False, {"status": response["status"]}
        keys = [resolve_sessions(client, key, quote=False) if isinstance(key, str) else key for key in spec.get("select", [])]
        value = scenario_steps.select(response["body"], keys)
    elif "file" in spec:
        path = Path(scenario_steps.substitute(spec["file"], context.places))
        if not path.is_file():
            value = None
        elif spec.get("encoding", "utf-8") == "bytes":
            data = path.read_bytes()
            value = {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}
        else:
            value = path.read_bytes().decode("utf-8")
    elif "git" in spec:
        cwd = scenario_steps.substitute(spec.get("cwd", "{fixture}"), context.places)
        result = subprocess.run(
            ["git", *spec["git"]], cwd=cwd, capture_output=True, timeout=15,
            env={"PATH": os.environ.get("PATH", ""), "HOME": cwd, "GIT_CONFIG_NOSYSTEM": "1",
                 "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_OPTIONAL_LOCKS": "0", "LANG": "C"},
        )
        if result.returncode != 0:
            return False, {"exitCode": result.returncode, "stderr": result.stderr.decode(errors="replace")[:500]}
        value = result.stdout.decode("utf-8")
    elif "gate" in spec:
        value = turn_gate(context)[spec["gate"]]
    else:
        value = sum(hit["id"] == spec["fault"] for hit in client.fault_hits())
        return value == spec["hits"], value
    if "where" in spec and isinstance(value, list):
        # Evidence keeps what the predicates judged.
        value = [item for item in value if scenario_steps.subset(spec["where"], item)]
    return scenario_steps.holds(spec, value), value


def wait_text(client, predicate, timeout_s: float) -> str | None:
    """The focused editable text once the predicate holds, else None."""
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        text = client.focused_text()
        if text is not None and predicate(text):
            return text
        time.sleep(0.1)
    return None


def perform(client, kind: str, steps: list[dict], targets: dict[str, dict], directory: Path, args: argparse.Namespace,
            context: StepContext | None = None) -> dict:
    """Runs the steps and returns the evidence of every checkpoint."""
    context = context or StepContext()
    checkpoints: dict[str, dict] = {}
    waits = context.waits

    def unmet(condition: dict, problem: str, **evidence) -> None:
        waits.append({"condition": condition, "met": False, **evidence})
        client.screenshot(directory / "wait-failed.png")
        raise TimeoutError(problem)

    for step in steps:
        if "wait" in step:
            condition = step["wait"]
            met = client.wait_for(condition[kind], float(condition.get("timeout", 30)))
            waits.append({"condition": condition[kind], "met": met})
            if not met:
                client.screenshot(directory / "wait-failed.png")
                raise TimeoutError(f"required wait failed before further input: {condition[kind]}")
        elif "click" in step:
            # Hydration can move rows between resolving a target and pressing
            # it. A quiet startup is a prerequisite for reproducible input.
            if not settle(client):
                pending = getattr(client, "pending_requests", lambda: [])()
                raise TimeoutError(f"the client never settled before row selection; pending route prefixes: {pending}")
            key = resolve_row(client.rows(), step["click"])
            client.click(key)
            selected = wait_selected(client, key)
            waits.append({"condition": {"selectedRow": key}, "met": selected})
            if not selected:
                client.screenshot(directory / "wait-failed.png")
                raise TimeoutError(f"required row selection failed before further input: {key}")
        elif "key" in step:
            client.key(step["key"])
        elif "clickTarget" in step:
            if not settle(client):
                pending = getattr(client, "pending_requests", lambda: [])()
                raise TimeoutError(f"the client never settled before control activation; pending route prefixes: {pending}")
            client.click_target(targets[step["clickTarget"]][kind])
        elif "clickNamed" in step:
            require_settled(client, "named control activation")
            client.click_named(scenario_steps.named_control(step["clickNamed"], kind))
        elif "hover" in step:
            require_settled(client, "hover")
            client.hover(targets[step["hover"]][kind])
        elif "drag" in step:
            require_settled(client, "drag")
            client.drag(targets[step["drag"]["from"]][kind], targets[step["drag"]["to"]][kind])
        elif "type" in step:
            client.type_text(step["type"])
        elif "paste" in step:
            value = step["paste"]
            text = value if isinstance(value, str) else value["text"]
            until = value.get("until") if isinstance(value, dict) else None

            # The clipboard is served by the probe's loop while the client
            # reads it, so the chord's completion is checked, never awaited.
            handled = []

            def pasted() -> bool:
                if handled and not handled[0]():
                    return False
                if until is not None:
                    return client.wait_for(until[kind], 0.05)
                current = client.focused_text()
                return current is not None and text in current

            try:
                context.clipboard.paste(text, lambda: handled.append(client.begin_key("ctrl+v")), pasted)
            except TimeoutError:
                unmet({"paste": until[kind] if until else "focused text contains the pasted text"},
                      "the paste never produced its text")
            waits.append({"condition": {"paste": until[kind] if until else "focused text contains the pasted text"}, "met": True})
        elif "ime" in step:
            value = step["ime"]
            before = client.focused_text()
            if "compose" in value:
                # The engine's preedit, when given, is what both clients must
                # show; otherwise any change of the text counts.
                preedit = value.get("preedit")
                context.composed = value["compose"]
                client.compose(value["compose"], preedit)
                condition = {"composing": value["compose"], **({"preedit": preedit} if preedit else {})}
                text = wait_text(client, (lambda current: preedit in current) if preedit else (lambda current: current != before), 10)
            else:
                committed = value["text"] if value["commit"] == "candidate" else context.composed
                client.commit_composition(value["commit"], committed)
                condition = {"committed": committed}
                text = wait_text(client, lambda current: committed in current, 10)
            if text is None:
                unmet(condition, f"the IME step never changed the focused text: {condition}", observed=client.focused_text())
            waits.append({"condition": condition, "met": True, "observed": text})
        elif "drop" in step:
            value = step["drop"]
            require_settled(client, "drop")
            dropped = client.drop(targets[value["target"]][kind], context.write_files(value["files"]))
            waits.append({"condition": {"dropped": [entry["name"] for entry in value["files"]]}, "met": True, "drop": dropped})
        elif "pick" in step:
            value = step["pick"]
            picked = client.pick(context.write_files(value["files"]), float(value.get("timeout", 15)))
            waits.append({"condition": {"picked": [entry["name"] for entry in value["files"]]}, "met": True, "chooser": picked})
        elif "fault" in step:
            rule = scenario_steps.fault_rule(step["fault"], kind)
            client.add_fault({**rule, "path": resolve_sessions(client, rule["path"])})
        elif "release" in step:
            client.release_fault(step["release"])
        elif "gate" in step:
            waits.append({"condition": {"gate": step["gate"]}, "met": True,
                          "observed": turn_gate(context, step["gate"] == "hold")})
        elif "restart" in step:
            value = step["restart"]
            record = client.restart(value.get("after", "signal"), float(value.get("timeout", 30)))
            context.restarts.append(record)
            if record["closed"].get("signal") == "timeout":
                raise TimeoutError(f"the client did not exit for the restart: {record['closed']}")
            client.wait_ready(args.timeout, connected=True)
        elif "expect" in step:
            spec = step["expect"]
            deadline = time.monotonic() + float(spec.get("timeout", 15))
            while True:
                try:
                    met, observed = observe(client, spec, context)
                except LookupError as error:
                    # A session the step names may not exist or be titled yet.
                    met, observed = False, {"unresolved": str(error)}
                if met or time.monotonic() >= deadline:
                    break
                time.sleep(0.25)
            if not met:
                unmet({"expect": spec}, f"expectation not met: {spec}", observed=scenario_steps.bounded(observed))
            waits.append({"condition": {"expect": spec}, "met": True, "observed": scenario_steps.bounded(observed)})
        elif "settle" in step:
            time.sleep(float(step["settle"]))
        elif "capture" in step:
            name = step["capture"]
            image = directory / f"{name}.png"
            settled = settle(client)
            client.screenshot(image)
            # The reference's text is where glyph-edge noise may be forgiven.
            text_boxes = getattr(client, "text_boxes", None)
            paint_reader = getattr(client, "paint_regions", None)
            paint = paint_reader() if paint_reader else {"regions": [], "protectedBoxes": []}
            checkpoints[name] = {
                "textBoxes": text_boxes() if text_boxes else [],
                "paintRegions": paint["regions"],
                "paintProtected": paint["protectedBoxes"],
                "composerGradientRegions": paint.get("composerGradientRegions", []),
                "composerGradientDefinitions": paint.get("composerGradientDefinitions", []),
                "composerGradientProtected": paint.get("composerGradientProtected", []),
                "composerGradientProtectionComplete": paint.get("composerGradientProtectionComplete", False),
                "image": str(image),
                "settled": settled,
                "pendingRoutes": [] if settled else getattr(client, "pending_requests", lambda: [])(),
                "rows": client.rows(),
                "targets": client.targets({target: targets[target][kind] for target in step.get("targets", [])}),
                "waits": list(waits),
                "substitutions": list(getattr(client, "substitutions", [])),
                "unobservable": sorted(getattr(client, "unobservable", ())),
                "faultHits": getattr(client, "fault_hits", list)(),
                "restarts": list(context.restarts),
            }
        else:
            raise ValueError(f"unknown step {step}")
    return checkpoints


def uses(scenario: dict, kind: str) -> bool:
    return any(kind in step for step in scenario["steps"])


def run_client(kind: str, scenario: dict, work: Path, args: argparse.Namespace) -> dict:
    """One client through the scenario. Services exist only for scenarios
    whose steps need them, so other scenarios launch exactly as before."""
    directory = work / kind
    directory.mkdir(parents=True)
    root = args.root.resolve()
    state = copy_seed(root, directory, args.theme, scenario.get("settings"), scenario.get("model"))
    started = time.monotonic()
    with contextlib.ExitStack() as services:
        try:
            places = {"project": str(root / "project")}
            if "fixture" in scenario:
                places["fixture"] = str(prepare_fixture(root, scenario["fixture"], state))
            context = StepContext(places, inputs=directory / "inputs")
            # GTK starts before the client so its initialization cannot take focus.
            if uses(scenario, "paste"):
                from clipboard_probe import ClipboardProbe

                context.clipboard = ClipboardProbe()
            if kind == "native":
                server = IsolatedServer(directory, args, scenario.get("provider"))
                services.callback(server.stop)
                server.wait_ready(args.timeout)
                context.fixture_port = server.fixture_port
                env = native_env(state, server.url, args.theme)
                client = NativeClient(args.native_binary, env, args.scale, args.width, args.height)
                client.server_url = server.url
                if uses(scenario, "fault"):
                    from fault_proxy import RuleFaultProxy

                    client.faults = RuleFaultProxy(server.url)
                    services.callback(client.faults.stop)
                    env["OPENCHAMBER_NATIVE_SERVER"] = client.faults.url
                if uses(scenario, "ime"):
                    from ime_fixture import IBusFixture

                    client.ibus = IBusFixture(directory / "ibus")
                    services.callback(client.ibus.close)
                    env.update(client.ibus.start())
                if uses(scenario, "drop"):
                    from drop_source import DropSource

                    client.drop_source = DropSource()
                if uses(scenario, "pick"):
                    from native_interactions import Interrupts
                    from portal_fixture import PortalFixture

                    interrupts = services.enter_context(Interrupts())
                    client.portal = PortalFixture(directory / "portal", env, interrupts)
                    services.callback(client.portal.close)
                    client.portal.start()
            else:
                fixture = FixtureProvider(directory, scenario.get("provider"))
                services.callback(fixture.stop)
                fixture.wait_ready(args.timeout)
                context.fixture_port = fixture.port
                client = ElectronClient(args.electron_app, electron_env(state, fixture.opencode_config), args.scale, args.width, args.height)
            try:
                launched = client.launch()
                client.wait_ready(args.timeout, connected=True)
                if kind == "electron" and uses(scenario, "pick"):
                    client.intercept_file_choosers()
                checkpoints = perform(client, kind, scenario["steps"], args.targets, directory, args, context)
            finally:
                closed = client.close()
            return {"launch": launched, "checkpoints": checkpoints, "close": closed, "seconds": round(time.monotonic() - started, 1),
                    "waits": context.waits, "restarts": context.restarts}
        except (RuntimeError, TimeoutError, LookupError, OSError, ValueError, subprocess.CalledProcessError) as error:
            return {"error": str(error)}


def comparable_rows(rows: list[dict]) -> list[dict]:
    """Draft IDs are client-generated; compare their group and displayed order."""
    drafts: dict[str, int] = {}
    result = []
    for row in rows:
        prefix, separator, draft = row["key"].rpartition(":draft:")
        if separator and draft and ":" not in draft:
            ordinal = drafts.get(prefix, 0)
            drafts[prefix] = ordinal + 1
            result.append({**row, "key": f"{prefix}:draft:{ordinal}"})
        else:
            result.append(row)
    return result


def compare_rows(reference: list[dict], candidate: list[dict], unobservable: frozenset[str] = frozenset(), unobserved: list[dict] | None = None) -> list[dict]:
    """Differences in row identity, order, label and state. A field the
    candidate's platform cannot report is listed in ``unobserved`` instead:
    neither a difference nor a match."""
    reference, candidate = comparable_rows(reference), comparable_rows(candidate)
    differences = []
    reference_keys = [row["key"] for row in reference]
    candidate_keys = [row["key"] for row in candidate]
    if reference_keys != candidate_keys:
        differences.append({
            "kind": "rows",
            "missing": [key for key in reference_keys if key not in candidate_keys],
            "extra": [key for key in candidate_keys if key not in reference_keys],
            "orderMatches": [key for key in reference_keys if key in candidate_keys] == [key for key in candidate_keys if key in reference_keys],
        })
    by_key = {row["key"]: row for row in candidate}
    for row in reference:
        other = by_key.get(row["key"])
        if other is None:
            continue
        for field in ("label", "selected", "expanded"):
            if field in unobservable:
                if row[field] is not None and unobserved is not None:
                    unobserved.append({"kind": field, "key": row["key"], "reference": row[field]})
                continue
            if row[field] != other[field]:
                differences.append({"kind": field, "key": row["key"], "reference": row[field], "candidate": other[field]})
    return differences


def artifact_identity(path: Path) -> dict:
    """Identify exact bytes and relative names without retaining file contents."""
    path = path.resolve(strict=True)

    def file_digest(item):
        content = hashlib.sha256()
        with item.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                content.update(chunk)
        return content.digest()

    if path.is_file():
        return {"path": str(path), "sha256": file_digest(path).hex(), "algorithm": "sha256", "files": 1}
    if not path.is_dir():
        raise OSError(f"comparison input is not a regular file or directory: {path}")
    files = sorted((item for item in path.rglob("*") if item.is_file()),
                   key=lambda item: item.relative_to(path).as_posix())
    digest = hashlib.sha256()
    for item in files:
        digest.update(item.relative_to(path).as_posix().encode())
        digest.update(b"\0")
        digest.update(file_digest(item))
    return {"path": str(path), "sha256": digest.hexdigest(), "algorithm": "sha256-tree-v1", "files": len(files)}


def scenario_file(args: argparse.Namespace) -> Path:
    """`scenarios.json`, or the runner's own step probes (`--scenarios-file`)."""
    return getattr(args, "scenarios_file", None) or SCENARIOS


def input_identities(args: argparse.Namespace) -> dict:
    electron_resources = args.electron_app.parent / "resources"
    electron_cli = "opencode.exe" if args.electron_app.suffix.lower() == ".exe" else "opencode"
    return {name: artifact_identity(path) for name, path in {
        "native": args.native_binary,
        "electron": args.electron_app,
        "electronPackage": electron_resources / "app.asar",
        "electronWebAssets": electron_resources / "web-dist",
        "electronOpencode": electron_resources / "opencode-cli" / electron_cli,
        "webAssets": args.dist,
        "opencode": args.opencode_binary,
        "seed": args.root / "seed",
        "scenarios": scenario_file(args),
        "inventory": INVENTORY,
        "exceptions": EXCEPTIONS,
    }.items()}


def run(args: argparse.Namespace) -> int:
    if not (args.root / "seed").exists():
        print(f"no seed under {args.root}; run the seed command first", file=sys.stderr)
        return 2
    scenario_source = scenario_file(args).read_bytes()
    inventory_source = INVENTORY.read_bytes()
    definitions = json.loads(scenario_source.decode("utf-8"))
    scenarios = definitions["scenarios"]
    args.targets = definitions.get("targets", {})
    features = json.loads(inventory_source.decode("utf-8"))["features"]
    linked = frozenset()
    if scenario_file(args) != SCENARIOS:
        linked = frozenset(scenario["id"] for scenario in json.loads(SCENARIOS.read_text())["scenarios"])
    problems = check_contracts(scenarios, features, args.targets, linked)
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        return 2
    wanted = {name.strip() for name in args.scenarios.split(",")} if args.scenarios else None
    unknown = wanted - {scenario["id"] for scenario in scenarios} if wanted is not None else set()
    if unknown:
        print(f"unknown parity scenarios: {', '.join(sorted(unknown))}", file=sys.stderr)
        return 2
    output = (args.output or REPO_ROOT / "artifacts" / f"parity-{time.strftime('%Y%m%dT%H%M%S')}").resolve()
    try:
        output.mkdir(parents=True)
    except FileExistsError:
        print(f"choose a new output directory; preserving {output}", file=sys.stderr)
        return 2
    try:
        inputs = input_identities(args)
    except OSError as error:
        print(f"cannot identify comparison inputs: {error}", file=sys.stderr)
        return 2
    if any(inputs.get(name, {}).get("sha256") != hashlib.sha256(source).hexdigest()
           for name, source in (("scenarios", scenario_source), ("inventory", inventory_source))):
        print("comparison declarations changed before the run; reload and retry", file=sys.stderr)
        return 2
    report = {"generatedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "configuration": {key: str(value) for key, value in vars(args).items()}, "inputs": inputs, "scenarios": {}}
    failed = 0
    previous_accessibility = native_adapter.accessibility_enabled()
    native_adapter.set_accessibility(True)
    try:
        failed = run_scenarios(scenarios, wanted, output, report, args)
    finally:
        native_adapter.set_accessibility(previous_accessibility)
        report["accessibilityRestoredTo"] = previous_accessibility
    try:
        unchanged = input_identities(args) == inputs
    except OSError:
        unchanged = False
    report["inputsUnchanged"] = unchanged
    if not unchanged:
        failed += 1
        for scenario in report["scenarios"].values():
            scenario["observedStatus"] = scenario["status"]
            scenario["status"] = "blocked"
        print("comparison inputs changed during the run; results are blocked", file=sys.stderr)
    report["inventory"] = inventory_coverage(features, report["scenarios"], scenarios)
    (output / "report.json").write_text(json.dumps(report, indent=2))
    coverage = report["inventory"]
    print(f"inventory: {coverage['passed']} of {coverage['features']} features passed; "
          f"{coverage['failed']} failed, {coverage['incomplete']} incomplete, {coverage['notRun']} not run, "
          f"{coverage['manual']} manual only")
    print(f"report: {output / 'report.json'}")
    inventory_refused = getattr(args, "require_complete_inventory", False) and (
        not coverage["features"] or coverage["passed"] != coverage["features"]
    )
    if inventory_refused:
        print_inventory_gaps(coverage)
    return 1 if failed or inventory_refused else 0


def check_contracts(scenarios: list[dict], features: list[dict], targets: dict[str, dict], linked: set[str] = frozenset()) -> list[str]:
    """Validate targets and explicit proofs against the feature requirements.

    Missing proof is a coverage gap, not an invalid contract. Unknown or
    unlinked proof cannot silently satisfy a requirement. `linked` names the
    `scenarios.json` IDs the inventory may list when another scenario file
    runs; the inventory never links probes, so a probe's proof is rejected.
    """
    feature_ids = {feature["id"] for feature in features}
    scenario_ids = {scenario["id"] for scenario in scenarios}
    problems = [f"scenario {scenario['id']} names unknown feature {scenario['feature']}" for scenario in scenarios if scenario["feature"] not in feature_ids]
    if len(feature_ids) != len(features):
        problems.append("duplicate feature IDs")
    if len(scenario_ids) != len(scenarios):
        problems.append("duplicate scenario IDs")
    by_feature = {feature["id"]: feature for feature in features}
    for scenario in scenarios:
        problems.extend(f"scenario {scenario['id']} {problem}" for problem in scenario_steps.check_scenario(scenario))
        for step in scenario["steps"]:
            problems.extend(f"scenario {scenario['id']} {problem}" for problem in scenario_steps.check_step(step, targets))
            problems.extend(
                f"scenario {scenario['id']} capture references undefined target {target}"
                for target in step.get("targets", [])
                if set(targets.get(target, {})) != {"electron", "native"}
            )
        for feature_id, proof in scenario.get("proofs", {}).items():
            feature = by_feature.get(feature_id)
            if feature is None:
                problems.append(f"scenario {scenario['id']} proves unknown feature {feature_id}")
                continue
            if scenario["id"] not in feature["scenarios"]:
                problems.append(f"scenario {scenario['id']} proof is not linked by feature {feature_id}")
            for field, requirement in (("behaviors", "requiredBehaviors"), ("states", "states")):
                values = proof.get(field, [])
                if not isinstance(values, list) or any(not isinstance(value, str) for value in values):
                    problems.append(f"scenario {scenario['id']} proof {feature_id}.{field} must be a string list")
                    continue
                for value in values:
                    if value not in feature.get(requirement, []):
                        problems.append(f"scenario {scenario['id']} proves unknown {field} {feature_id}:{value}")
            if set(proof) - {"behaviors", "states"}:
                problems.append(f"scenario {scenario['id']} proof {feature_id} has unknown fields")
    for feature in features:
        for field in ("requiredBehaviors", "states"):
            values = feature.get(field)
            if (not isinstance(values, list) or not values
                    or any(not isinstance(value, str) or not value.strip() for value in values)):
                problems.append(f"feature {feature['id']} needs nonempty {field}")
            elif len(set(values)) != len(values):
                problems.append(f"feature {feature['id']} has duplicate {field}")
        if feature.get("coverage") not in ("automated", "manual"):
            problems.append(f"feature {feature['id']} needs automated or manual coverage")
        if feature.get("coverage") == "manual" and not feature.get("manualCheck"):
            problems.append(f"feature {feature['id']} needs a manualCheck procedure")
        problems.extend(f"feature {feature['id']} lists unknown scenario {scenario}" for scenario in feature["scenarios"] if scenario not in scenario_ids | linked)
    return problems


def inventory_coverage(features: list[dict], results: dict[str, dict], scenarios: list[dict]) -> dict:
    """Union proofs from the executed contract snapshot, retaining every gap."""
    by_scenario = {scenario["id"]: scenario for scenario in scenarios}
    counts = {"features": len(features), "passed": 0, "failed": 0, "incomplete": 0, "notRun": 0, "manual": 0, "byFeature": {}, "missingRequirements": {}}
    for feature in features:
        statuses = [results.get(scenario, {}).get("status", "not run") for scenario in feature["scenarios"]]
        proved = {"behaviors": set(), "states": set()}
        for scenario_id in feature["scenarios"]:
            if results.get(scenario_id, {}).get("status") != "passed":
                continue
            proof = by_scenario.get(scenario_id, {}).get("proofs", {}).get(feature["id"], {})
            for field in proved:
                proved[field].update(proof.get(field, []))
        missing = {
            "behaviors": [value for value in feature["requiredBehaviors"] if value not in proved["behaviors"]],
            "states": [value for value in feature["states"] if value not in proved["states"]],
        }
        counts["missingRequirements"][feature["id"]] = missing
        if any(status in ("failed", "blocked") for status in statuses):
            status = "failed"
        elif feature["coverage"] == "manual":
            status = "manual"
        elif (statuses and all(status == "passed" for status in statuses)
              and feature["requiredBehaviors"] and feature["states"]
              and not any(missing.values())):
            status = "passed"
        elif any(status in ("passed", "incomplete") for status in statuses):
            status = "incomplete"
        else:
            status = "not run"
        counts["byFeature"][feature["id"]] = status
        counts[{"passed": "passed", "failed": "failed", "incomplete": "incomplete", "not run": "notRun", "manual": "manual"}[status]] += 1
    return counts


def print_inventory_gaps(coverage: dict) -> None:
    """Keep terminal diagnostics bounded; the report retains every requirement."""
    if not coverage["features"]:
        print("complete inventory required: the inventory has no features", file=sys.stderr)
        return
    gaps = [(feature, status) for feature, status in coverage["byFeature"].items() if status != "passed"]
    print(f"complete inventory required: {len(gaps)} of {coverage['features']} features did not pass", file=sys.stderr)

    def summary(values):
        shown = [value if len(value) <= 80 else value[:77] + "..." for value in values[:3]]
        if len(values) > 3:
            shown.append(f"+{len(values) - 3} more")
        return ", ".join(shown) or "none"

    for feature, status in gaps[:8]:
        missing = coverage["missingRequirements"][feature]
        print(f"  {feature}: {status}; missing behaviors: {summary(missing['behaviors'])}; "
              f"missing states: {summary(missing['states'])}", file=sys.stderr)
    if len(gaps) > 8:
        print(f"  {len(gaps) - 8} more features did not pass", file=sys.stderr)
    print("see report.json inventory for every feature and missing requirement", file=sys.stderr)


def approved_exceptions(kind: str) -> set[tuple[str, str]]:
    """The checkpoints whose noise category the user approved, from
    ``exceptions.json``; each entry names its region, reason and decision."""
    document = json.loads(EXCEPTIONS.read_text())
    approved = set()
    for entry in document.get(kind, []):
        if not all(entry.get(key) for key in ("scenario", "checkpoint", "region", "reason", "decision")):
            raise ValueError(f"an exception needs its scenario, checkpoint, region, reason and decision: {entry}")
        if kind == "paintNoise" and entry.get("maxChannelDelta") != compare.PAINT_SLACK:
            raise ValueError("paint noise is limited to the approved channel delta")
        if kind == "composerGradientNoise" and entry.get("maxChannelDelta") != compare.COMPOSER_GRADIENT_SLACK:
            raise ValueError("composer gradient noise is limited to one RGB level")
        approved.add((entry["scenario"], entry["checkpoint"]))
    return approved


def composer_gradient_masks(reference: dict, candidate: dict, scale: float) -> tuple[list, list]:
    """Require blank coverage from both clients; missing metadata grants nothing."""
    if not all(checkpoint.get("composerGradientProtectionComplete") is True
               and checkpoint.get("composerGradientRegions")
               and checkpoint.get("composerGradientProtected")
               for checkpoint in (reference, candidate)):
        return [], []
    regions = []
    for left in reference.get("composerGradientRegions", []):
        for right in candidate.get("composerGradientRegions", []):
            x, y = max(left[0], right[0]), max(left[1], right[1])
            width = min(left[0] + left[2], right[0] + right[2]) - x
            height = min(left[1] + left[3], right[1] + right[3]) - y
            if width > 0 and height > 0:
                regions.append([value * scale for value in (x, y, width, height)])
    protected = [box for checkpoint in (reference, candidate)
                 for box in checkpoint.get("composerGradientProtected", []) + checkpoint.get("textBoxes", [])]
    return regions, [[value * scale for value in box] for box in protected]


def run_scenarios(scenarios: list[dict], wanted: set[str] | None, output: Path, report: dict, args: argparse.Namespace) -> int:
    failed = 0
    for scenario in scenarios:
        if wanted and scenario["id"] not in wanted:
            continue
        work = output / scenario["id"]
        result = {"feature": scenario["feature"], "electron": run_client("electron", scenario, work, args), "native": run_client("native", scenario, work, args)}
        if "error" in result["electron"] or "error" in result["native"]:
            result["status"] = "blocked" if "error" in result["electron"] else "failed"
        else:
            comparisons = {}
            for name, reference in result["electron"]["checkpoints"].items():
                candidate = result["native"]["checkpoints"].get(name)
                if candidate is None:
                    comparisons[name] = {"missing": True}
                    continue
                text_boxes = None
                if (scenario["id"], name) in approved_exceptions("glyphEdges"):
                    text_boxes = [[value * args.scale for value in box] for box in reference.get("textBoxes", [])]
                paint_regions, protected = [], []
                if (scenario["id"], name) in approved_exceptions("paintNoise"):
                    paint_regions = [[value * args.scale for value in region["box"]] for region in reference.get("paintRegions", [])]
                    protected = [[value * args.scale for value in box] for box in reference.get("paintProtected", []) + reference.get("textBoxes", [])]
                composer_regions, composer_protected = [], []
                if (scenario["id"], name) in approved_exceptions("composerGradientNoise"):
                    composer_regions, composer_protected = composer_gradient_masks(reference, candidate, args.scale)
                image = compare.compare_images(Path(reference["image"]), Path(candidate["image"]), work / name, args.tolerance, text_boxes, paint_regions, protected, composer_regions, composer_protected)
                rects_reference = {row["key"]: row["rect"] for row in comparable_rows(reference["rows"])}
                rects_candidate = {row["key"]: row["rect"] for row in comparable_rows(candidate["rows"])}
                for target, rect in reference.get("targets", {}).items():
                    rects_reference[f"target:{target}"] = rect
                for target, rect in candidate.get("targets", {}).items():
                    rects_candidate[f"target:{target}"] = rect
                unobserved: list[dict] = []
                rows = compare_rows(reference["rows"], candidate["rows"], frozenset(candidate.get("unobservable", ())), unobserved)
                # An input the host could not deliver was replaced; its path is unverified.
                unobserved.extend(candidate.get("substitutions", []))
                comparisons[name] = {
                    "image": image.as_dict(),
                    "rows": rows,
                    "unobserved": unobserved,
                    "geometry": compare.compare_geometry(rects_reference, rects_candidate, args.scale),
                }
            for name, comparison in comparisons.items():
                if not comparison.get("missing"):
                    candidate = result["native"]["checkpoints"][name]
                    comparison["unmetWaits"] = [wait for wait in candidate.get("waits", []) if not wait["met"]]
                    comparison["nativeUnsettled"] = not candidate.get("settled", True)
            result["comparisons"] = comparisons
            # A reference that never reached its state, or never held still,
            # is no reference.
            reference_unmet = any(
                not wait["met"] for checkpoint in result["electron"]["checkpoints"].values() for wait in checkpoint.get("waits", [])
            ) or any(not checkpoint.get("settled", True) for checkpoint in result["electron"]["checkpoints"].values())
            passed = all(
                not comparison.get("missing")
                and comparison["image"]["sizeMatches"]
                and comparison["image"]["differingPixels"] == 0
                and not comparison["rows"]
                and not comparison["geometry"]
                and not comparison["unmetWaits"]
                and not comparison["nativeUnsettled"]
                for comparison in comparisons.values()
            )
            # Checks the candidate's platform cannot observe leave a clean
            # scenario incomplete: unverified is never passed.
            unverified = any(comparison.get("unobserved") for comparison in comparisons.values())
            if reference_unmet:
                result["status"] = "blocked"
            else:
                result["status"] = "failed" if not passed else "incomplete" if unverified else "passed"
        failed += result["status"] != "passed"
        report["scenarios"][scenario["id"]] = result
        print(f"{scenario['id']}: {result['status']}", flush=True)
        for name, comparison in result.get("comparisons", {}).items():
            if comparison.get("missing"):
                print(f"  {name}: no native checkpoint")
                continue
            image = comparison["image"]
            print(f"  {name}: {image['differingPixels']} differing pixels ({image['differingShare']:.1%}), "
                  f"{image['glyphEdgePixels']} glyph-edge pixels, "
                  f"{image['paintNoisePixels']} shadow/gradient pixels, "
                  f"{image['composerGradientNoisePixels']} composer-gradient pixels, "
                  f"{len(comparison['rows'])} row differences, {len(comparison['geometry'])} geometry differences, "
                  f"{len(comparison['unobserved'])} checks not observable natively, "
                  f"{len(comparison['unmetWaits'])} native waits not met"
                  + (", native never settled" if comparison["nativeUnsettled"] else ""))
        for kind in ("electron", "native"):
            if "error" in result[kind]:
                print(f"  {kind}: {result[kind]['error']}")
    return failed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("seed", "run"):
        command = commands.add_parser(name)
        command.add_argument("--root", type=Path, default=REPO_ROOT / "tmp" / "parity", help="seed and project directory")
        command.add_argument("--dist", type=Path, default=DEFAULT_DIST, help="built web UI the isolated server serves")
        command.add_argument("--opencode-binary", type=Path, default=DEFAULT_OPENCODE)
        command.add_argument("--timeout", type=float, default=120.0)
    commands.choices["seed"].add_argument("--force", action="store_true", help="rebuild an existing seed")
    runner = commands.choices["run"]
    runner.add_argument("--scenarios", help="comma-separated scenario IDs; all by default")
    runner.add_argument("--scenarios-file", type=Path, help="scenario file other than scenarios.json, such as step_probes.json")
    runner.add_argument(
        "--require-complete-inventory", action="store_true",
        help="fail unless every inventory feature passes with complete behavior and state proofs",
    )
    runner.add_argument("--electron-app", type=Path, default=DEFAULT_ELECTRON)
    runner.add_argument("--native-binary", type=Path, default=DEFAULT_NATIVE)
    runner.add_argument("--theme", default="openchamber-dark")
    runner.add_argument("--width", type=int, default=1280)
    runner.add_argument("--height", type=int, default=800)
    runner.add_argument("--scale", type=float, default=1.0)
    runner.add_argument("--tolerance", type=int, default=0, help="largest per-channel difference ignored; reviewed noise only")
    runner.add_argument("--output", type=Path, help="report directory, outside the repository's sources")
    runner.add_argument(
        "--display", choices=("private",), default="private",
        help="run on an owned Xvfb display with a private accessibility bus",
    )
    args = parser.parse_args()
    if args.command == "seed":
        return seed(args)
    if args.display == "private" and not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            print(f"no private display: {problem}", file=sys.stderr)
            return 1
        command = [sys.executable, "-u", str(Path(__file__).resolve()), *sys.argv[1:]]
        return virtual_display.run_inside(command, args.width, args.height)
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
