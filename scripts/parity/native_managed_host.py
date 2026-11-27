#!/usr/bin/env python3
"""The managed local host, end to end on a private display.

The native client starts with no configured host, so it starts the bundled
server under the native bundle's Bun runtime. The check sends a prompt through the
fixture provider, kills the managed server and waits for recovery, quits
through the window's close button and verifies that no process the launch
owned remains, while an external server started here keeps running. A second
launch keeps the port; a launch without a bundle shows the failure screen.
Opening the external server displays its own session and leaves it running on close.

    python3 scripts/parity/native_managed_host.py --binary <openchamber-native> \\
        --bundle <native-bundle-directory> \\
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
    return start is not None and process_identity(pid) == start


def signal_owned(pid, start, sig):
    if not alive(pid, start):
        return False
    try:
        descriptor = os.pidfd_open(pid)
        try:
            if not alive(pid, start):
                return False
            signal.pidfd_send_signal(descriptor, sig)
            return True
        finally:
            os.close(descriptor)
    except ProcessLookupError:
        return False


def environ_mentions(pid, needle):
    try:
        return needle in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
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

    def __init__(self, bundle, root, opencode_config):
        self.port = runner.free_port()
        self.project = root / "project"
        prepare_state(root, self.project)
        resources = bundle / "resources"
        env = {key: os.environ[key] for key in ("PATH", "LANG") if key in os.environ}
        env.update(runner.state_dirs(root))
        env.update(OPENCHAMBER_DATA_DIR=str(root / "oc-data"), OPENCODE_CONFIG_CONTENT=opencode_config,
                   OPENCHAMBER_BUNDLED_OPENCODE_CLI_DIR=str(resources / "opencode-cli"),
                   OPENCHAMBER_OPENCODE_CWD=str(self.project),
                   OPENCHAMBER_DIST_DIR=str(resources / "web-dist"), SHELL="/bin/sh")
        self.log = (root / "external.log").open("w")
        package = resources / "server"
        self.process = subprocess.Popen([str(resources / "bun/bun"), "--cwd", str(package), "--no-env-file",
                                         "--no-install", str(package / "server/index.js"), "--port", str(self.port), "--host", "127.0.0.1"],
                                         env=env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=subprocess.STDOUT,
                                         start_new_session=True)
        self.url = f"http://127.0.0.1:{self.port}"
        try:
            self.engine = until("the external server did not start its bundled OpenCode",
                                lambda: self.ready_engine(resources / "opencode-cli/opencode"), 90)
            self.session = create_session(self.url, self.project, "External host")
        except BaseException:
            self.stop()
            raise

    def ready_engine(self, executable):
        if self.process.poll() is not None:
            raise RuntimeError("the external server exited during startup")
        try:
            health = http_json(f"{self.url}/health", 2)
        except OSError:
            return None
        if health.get("opencodeBinarySource") == "bundled" and health.get("isOpenCodeReady") is True:
            for pid in children(self.process.pid):
                start = process_identity(pid)
                if start is not None and program_matches(pid, executable):
                    return pid, start
        return None

    def check_session(self):
        if self.process.poll() is not None or not alive(*self.engine):
            raise RuntimeError("closing the native client stopped the external server or its OpenCode")
        data = http_json(f"{self.url}/api/session/{self.session}")["data"]
        if data.get("id") != self.session or data.get("title") != "External host":
            raise RuntimeError("the external server lost its session")

    def stop(self):
        # Its OpenCode leads a group of its own; stop whatever is left of it.
        engine = getattr(self, "engine", None)
        owned = descendants(self.process.pid) if self.process.poll() is None else {}
        if engine:
            owned[engine[0]] = engine[1]
        try:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=40)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=10)
        finally:
            self.log.close()
            for pid, start in owned.items():
                signal_owned(pid, start, signal.SIGKILL)


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


def client_env(state, opencode_config, bundle):
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
        OPENCODE_CONFIG_CONTENT=opencode_config,
    )
    return env


def program_matches(pid, expected):
    try:
        return Path(f"/proc/{pid}/exe").samefile(expected)
    except (FileNotFoundError, ProcessLookupError):
        return False


def managed_server(client, bun):
    """The native client's owned server: its only child running the runtime."""
    def find():
        servers = [pid for pid in children(client.process.pid) if program_matches(pid, bun)]
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


def create_session(url, project, title):
    body = json.dumps({"title": title, "agent": "build",
                       "model": {"providerID": "perf", "id": "stream-300cps"},
                       "location": {"directory": str(project)}}).encode()
    created = urllib.request.Request(f"{url}/api/session", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(created, timeout=15) as response:
        return json.load(response)["data"]["id"]


def send_prompt(client, url, project):
    """Opens a new session in the window and sends the prompt from its composer."""
    session = create_session(url, project, "Managed host")
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
    code = client.process.wait(timeout=timeout)
    if code != 0:
        raise RuntimeError(f"the native client exited with status {code} on window close")
    return code


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {"passed": False, "binary": identity, "display": "owned-xvfb", "checks": []}
    checks = report["checks"]
    enabled = accessibility_enabled()
    bundle = args.bundle
    bun = bundle / "resources/bun/bun"
    try:
        set_accessibility(True)
        with Interrupts(), ExitStack() as cleanup:
            fixture = runner.FixtureProvider(args.output)
            cleanup.callback(fixture.stop)
            fixture.wait_ready(20)
            external = ExternalServer(bundle, args.output / "external", fixture.opencode_config)
            cleanup.callback(external.stop)

            state = args.output / "state"
            project = args.output / "project"
            prepare_state(state, project)
            env = client_env(state, fixture.opencode_config, bundle)
            needle = f"OPENCHAMBER_DATA_DIR={state / 'oc-data'}".encode()

            log = cleanup.enter_context((args.output / "native.log").open("w"))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(120, connected=True)
            port = json.loads((Path(env["XDG_CONFIG_HOME"]) / "openchamber-native/managed-host.json").read_text())["port"]
            url = f"http://127.0.0.1:{port}"
            server, server_start = managed_server(client, bun)
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
            if not any(program_matches(pid, bundle / "resources/opencode-cli/opencode") for pid in first_opencode):
                raise RuntimeError("the managed Bun server runs no bundled OpenCode")

            if not signal_owned(server, server_start, signal.SIGKILL):
                raise RuntimeError("the managed Bun server exited before the crash check")
            until("the window never said the server is restarting",
                  lambda: status_text(client) == "Server restarting...", 15)
            client.screenshot(args.output / "managed-recovering.png")
            replacement, replacement_start = managed_server(client, bun)
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
            external.check_session()
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
            relaunched, _ = managed_server(client, bun)
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

            remote_env = runner.native_env(args.output / "remote-state", external.url, "openchamber-dark")
            remote_env["OPENCHAMBER_NATIVE_BUNDLE"] = str(bundle)
            log4 = cleanup.enter_context((args.output / "native-external.log").open("w"))
            remote = NativeClient(args.binary, remote_env, args.scale, 1280, 800, stderr=log4)
            cleanup.callback(remote.close)
            remote.launch()
            remote.wait_ready(120, connected=True)
            until("the external session did not appear", lambda: any(
                row["key"].endswith(":session:" + external.session) for row in remote.rows()), 30)
            if any(program_matches(pid, bun) for pid in children(remote.process.pid)):
                raise RuntimeError("the native client started a managed server in external mode")
            quit_by_close(remote, 60)
            external.check_session()
            if remote.substitutions:
                raise RuntimeError("external window close used an accessibility substitute")
            checks.append("connected-external-survives")
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
    parser.add_argument("--bundle", type=Path, help="native package root (default: beside the binary)")
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    args = parser.parse_args()
    args.bundle = args.bundle or args.binary.parent
    for key in ("binary", "output", "bundle"):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path("/tmp/opencode") or args.output.exists():
        parser.error("--output must be a new directory directly under /tmp/opencode")
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    raise SystemExit(main())
