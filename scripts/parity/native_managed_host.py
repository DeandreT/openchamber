#!/usr/bin/env python3
"""The managed local host, end to end on a private display.

The native client starts with no configured host, so it starts the bundled
server from an Electron package layout. The check sends a prompt through the
fixture provider, kills the managed server and waits for recovery, quits
through the window's close button and verifies that no process the launch
owned remains, while an external server started here keeps running. A second
launch keeps the port; a launch without a bundle shows the failure screen.

    python3 scripts/parity/native_managed_host.py --binary <openchamber-native> \\
        --electron-app <linux-unpacked>/openchamber --opencode-binary <opencode> \\
        --output /tmp/opencode/<new directory>
"""

import argparse
import base64
from contextlib import ExitStack
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

import runner
import virtual_display
from native_adapter import NativeClient, _atspi
from native_interactions import Interrupts, accessibility_enabled, process_identity, set_accessibility, until
from native_platform import binary_info

PROMPT = "Managed host fixture prompt."


def http_json(url, timeout=5):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return json.load(response)


def healthy(url):
    try:
        return http_json(f"{url}/health", 2).get("status") == "ok"
    except OSError:
        return False


def parents():
    """Every process's parent, from its stat line."""
    found = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
        except (FileNotFoundError, ProcessLookupError):
            continue
        found[int(entry.name)] = int(fields[1])
    return found


def children(pid):
    """Children of every thread of `pid`; `task/<pid>/children` lists only
    the main thread's, and the supervisor spawns from its own."""
    return [child for child, parent in parents().items() if parent == pid]


def descendants(pid):
    """`{pid: start}` of `pid` and everything below it."""
    links = parents()
    found = {pid: process_identity(pid)}
    pending = [pid]
    while pending:
        current = pending.pop()
        for child, parent in links.items():
            if parent == current and child not in found:
                found[child] = process_identity(child)
                pending.append(child)
    return {child: start for child, start in found.items() if start is not None}


def alive(pid, start):
    return process_identity(pid) == start


def environ_mentions(pid, needle):
    try:
        return needle in Path(f"/proc/{pid}/environ").read_bytes()
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return False


def processes_with(needle):
    """Every process whose environment names this launch's state."""
    found = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit() and int(entry.name) != os.getpid() and environ_mentions(entry.name, needle):
            found.append(int(entry.name))
    return found


def loopback_only(port):
    """The listeners on `port`, which must all be 127.0.0.1."""
    addresses = []
    for table in ("/proc/net/tcp", "/proc/net/tcp6"):
        for line in Path(table).read_text().splitlines()[1:]:
            fields = line.split()
            local, state = fields[1], fields[3]
            address, hex_port = local.rsplit(":", 1)
            if state == "0A" and int(hex_port, 16) == port:
                addresses.append(address)
    return addresses and all(address == "0100007F" for address in addresses), addresses


class ExternalServer:
    """A server this script owns and the native client must never stop."""

    def __init__(self, app, root):
        self.port = runner.free_port()
        root.mkdir(parents=True)
        resources = app.parent / "resources"
        env = {key: os.environ[key] for key in ("PATH", "LANG") if key in os.environ}
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
                   XDG_CACHE_HOME=str(root / "cache"), XDG_STATE_HOME=str(root / "state"),
                   OPENCHAMBER_DATA_DIR=str(root / "oc-data"), OPENCHAMBER_SKIP_OPENCODE_START="true",
                   OPENCHAMBER_DIST_DIR=str(resources / "web-dist"), ELECTRON_RUN_AS_NODE="1", SHELL="/bin/sh")
        self.log = (root / "external.log").open("w")
        entry = resources / "app.asar/node_modules/@openchamber/web/server/index.js"
        self.process = subprocess.Popen([str(app), str(entry), "--port", str(self.port), "--host", "127.0.0.1"],
                                        env=env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=subprocess.STDOUT,
                                        start_new_session=True)
        self.url = f"http://127.0.0.1:{self.port}"
        until("the external server did not start", lambda: healthy(self.url), 60)

    def stop(self):
        # Its OpenCode leads a group of its own; stop whatever is left of it.
        tree = descendants(self.process.pid)
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=40)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=10)
        for pid, start in tree.items():
            if alive(pid, start):
                os.kill(pid, signal.SIGKILL)
        self.log.close()


def prepare_state(state, project):
    for directory in runner.state_dirs(state).values():
        Path(directory).mkdir(parents=True, exist_ok=True)
    project.mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=project, check=True)
    data = state / "oc-data"
    data.mkdir()
    identifier = "path_" + base64.urlsafe_b64encode(str(project).encode()).decode().rstrip("=")
    now = int(time.time() * 1000)
    (data / "settings.json").write_text(json.dumps({
        "homeDirectory": str(state / "home"),
        "projects": [{"id": identifier, "path": str(project), "label": project.name,
                      "addedAt": now, "lastOpenedAt": now, "sidebarCollapsed": False}],
        "activeProjectId": identifier,
        "lastDirectory": str(project),
        "defaultModel": "perf/stream-300cps",
    }, indent=2))


def client_env(state, args, opencode_config, bundle):
    env = {key: os.environ[key] for key in runner.CLIENT_ENV_KEYS if key in os.environ}
    env.update(runner.state_dirs(state))
    env.update(
        SHELL="/bin/sh",
        LANG="en_US.UTF-8",
        OPENCHAMBER_DATA_DIR=str(state / "oc-data"),
        OPENCHAMBER_NATIVE_BUNDLE=str(bundle),
        OPENCHAMBER_NATIVE_THEME="openchamber-dark",
        OPENCHAMBER_NATIVE_LOCALE="en",
        OPENCHAMBER_NATIVE_LOG="info",
        OPENCODE_BINARY=str(args.opencode_binary),
        OPENCODE_CONFIG_CONTENT=opencode_config,
    )
    return env


def managed_server(client):
    """The native client's owned server: its only child running the runtime."""
    def find():
        servers = [pid for pid in children(client.process.pid)
                   if Path(f"/proc/{pid}/comm").read_text().strip() == "openchamber"]
        return servers[0] if len(servers) == 1 else None
    pid = until("the native client started no server", find, 30)
    return pid, process_identity(pid)


def status_text(client):
    try:
        return client._find("connection-status").get_name()
    except LookupError:
        return None


def composer_entry(client):
    try:
        root = client._find("composer")
    except LookupError:
        return None
    stack = [root]
    while stack:
        node = stack.pop()
        if node is None:
            continue
        if node.get_role_name() == "entry":
            return node
        stack.extend(node.get_child_at_index(index) for index in range(node.get_child_count()))
    return None


def send_prompt(client, url, project):
    """Creates a session in the project, opens it in the window and sends the
    prompt from the composer."""
    body = json.dumps({"title": "Managed host", "agent": "build",
                       "model": {"providerID": "perf", "id": "stream-300cps"},
                       "location": {"directory": str(project)}}).encode()
    created = urllib.request.Request(f"{url}/api/session", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(created, timeout=15) as response:
        session = json.load(response)["data"]["id"]
    key = until("the new session's row did not appear", lambda: next(
        (row["key"] for row in client.rows() if row["key"].endswith(":session:" + session)), None), 30)
    client.click(key)
    if not runner.wait_selected(client, key):
        raise RuntimeError("the session did not open")
    entry = until("the composer did not appear", lambda: composer_entry(client), 30)
    client._click_node(entry, "composer")
    time.sleep(0.2)
    client._xdotool("type", "--delay", "20", PROMPT)
    until("send did not enable", lambda: client._find("composer-submit").get_state_set()
          .contains(_atspi().StateType.ENABLED), 15)
    client.key("Return")

    def answered():
        messages = http_json(f"{url}/api/session/{session}/message?limit=100")["data"]
        prompted = any(m.get("type") == "user" and m.get("text") == PROMPT for m in messages)
        replied = any(m.get("type") == "assistant" and any(
            part.get("type") == "text" and part.get("text") for part in m.get("content") or [])
            for m in messages)
        return prompted and replied
    until("the fixture provider's answer did not arrive", answered, 90)
    return session


def quit_by_close(client, timeout):
    client.click("window-close")
    client.process.wait(timeout=timeout)
    return client.process.returncode


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {"passed": False, "binary": identity, "display": "owned-xvfb", "checks": []}
    checks = report["checks"]
    enabled = accessibility_enabled()
    bundle = args.electron_app.parent
    try:
        set_accessibility(True)
        with Interrupts(), ExitStack() as cleanup:
            fixture = runner.FixtureProvider(args.output)
            cleanup.callback(fixture.stop)
            fixture.wait_ready(20)
            external = ExternalServer(args.electron_app, args.output / "external")
            cleanup.callback(external.stop)
            external_start = process_identity(external.process.pid)

            state = args.output / "state"
            project = args.output / "project"
            prepare_state(state, project)
            env = client_env(state, args, fixture.opencode_config, bundle)
            needle = f"OPENCHAMBER_DATA_DIR={state / 'oc-data'}".encode()

            log = cleanup.enter_context((args.output / "native.log").open("w"))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(120, connected=True)
            port = json.loads((Path(env["XDG_CONFIG_HOME"]) / "openchamber-native/managed-host.json").read_text())["port"]
            url = f"http://127.0.0.1:{port}"
            server, server_start = managed_server(client)
            health = http_json(f"{url}/health")
            if health.get("runtime") != "desktop":
                raise RuntimeError(f"the managed server runs as {health.get('runtime')}")
            only_loopback, listeners = loopback_only(port)
            if not only_loopback:
                raise RuntimeError(f"the managed server listens beyond loopback: {listeners}")
            checks += ["managed-start", "desktop-runtime", "loopback-listener"]
            client.screenshot(args.output / "managed-ready.png")

            session = send_prompt(client, url, project)
            checks.append("prompt-through-fixture")
            owned = descendants(server)
            first_opencode = {pid: start for pid, start in owned.items() if pid != server}
            if not first_opencode:
                raise RuntimeError("the managed server runs no OpenCode")

            os.kill(server, signal.SIGKILL)
            until("the window never said the server is restarting",
                  lambda: status_text(client) == "Server restarting...", 15)
            client.screenshot(args.output / "managed-recovering.png")
            replacement, replacement_start = managed_server(client)
            if (replacement, replacement_start) == (server, server_start):
                raise RuntimeError("the killed server was not replaced")
            until("the restarted server did not become healthy", lambda: healthy(url), 60)
            until("the window did not reconnect", lambda: status_text(client) is None
                  and any(row["key"].endswith(":session:" + session) for row in client.rows()), 60)
            until("the crashed server's OpenCode was not reaped",
                  lambda: not any(alive(pid, start) for pid, start in first_opencode.items()), 60)
            checks += ["crash-recovering", "crash-recovered", "orphan-reaped"]

            owned = descendants(replacement)
            started = time.monotonic()
            exit_code = quit_by_close(client, 60)
            report["quitSeconds"] = round(time.monotonic() - started, 2)
            report["exitCode"] = exit_code
            survivors = [pid for pid, start in owned.items() if alive(pid, start)]
            survivors += [pid for pid in processes_with(needle) if pid not in survivors]
            if survivors:
                raise RuntimeError(f"owned processes survived quit: {survivors}")
            if not alive(external.process.pid, external_start) or not healthy(external.url):
                raise RuntimeError("quitting stopped the external server")
            checks += ["quit-stops-owned", "external-survives"]

            log2 = cleanup.enter_context((args.output / "native-relaunch.log").open("w"))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log2)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(120, connected=True)
            again = json.loads((Path(env["XDG_CONFIG_HOME"]) / "openchamber-native/managed-host.json").read_text())["port"]
            if again != port:
                raise RuntimeError(f"the relaunch moved the server from {port} to {again}")
            until("the relaunch did not show the earlier session",
                  lambda: any(row["key"].endswith(":session:" + session) for row in client.rows()), 30)
            relaunched, _ = managed_server(client)
            owned = descendants(relaunched)
            quit_by_close(client, 60)
            if any(alive(pid, start) for pid, start in owned.items()) or processes_with(needle):
                raise RuntimeError("owned processes survived the relaunched quit")
            checks += ["relaunch-same-port", "relaunch-quit"]

            missing = dict(env, OPENCHAMBER_NATIVE_BUNDLE=str(args.output / "no-bundle"))
            log3 = cleanup.enter_context((args.output / "native-missing.log").open("w"))
            client = NativeClient(args.binary, missing, args.scale, 1280, 800, stderr=log3)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(60, connected=False)
            if not client.wait_for({"elements": ["managed-host-recovery", "managed-host-retry",
                                                 "managed-host-remote"]}, 15):
                raise RuntimeError("a missing bundle did not show the failure screen")
            client.screenshot(args.output / "managed-failed.png")
            if children(client.process.pid):
                raise RuntimeError("a missing bundle still started a process")
            quit_by_close(client, 30)
            checks.append("missing-bundle-failure")
            if client.substitutions:
                raise RuntimeError("pointer input used an accessibility substitute")
        report["passed"] = True
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        set_accessibility(enabled)
        if binary_info(args.binary) != identity:
            report.update(passed=False, error="candidate changed during qualification")
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--electron-app", type=Path, default=runner.DEFAULT_ELECTRON)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    args = parser.parse_args()
    for key in ("binary", "output", "electron_app", "opencode_binary"):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path("/tmp/opencode") or args.output.exists():
        parser.error("--output must be a new directory directly under /tmp/opencode")
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    raise SystemExit(main())
