#!/usr/bin/env python3
"""Records native desktop timings and separate Electron reference diagnostics.

Scenarios:

- ``startup``: process spawn to the native ``composer-usable`` marker, with the
  window, paint and sidebar markers alongside.
- ``memory``: proportional and private bytes of the whole owned process tree
  after startup settles and after the switch sequence.
- ``idle``: CPU time of the owned process tree over a quiet interval.
- ``switch``: AT-SPI dispatch to paint-marker receipt after explicit setup,
  in forward and reverse order. First connected visits also wait for history.
  Needs PyGObject with AT-SPI; the session accessibility flag is enabled for
  the run and restored afterwards.

With ``--electron-app`` the packaged Electron build runs through
``scripts/profile-startup.mjs`` and a separate memory launch, alternating with
the native runs (A/B/B/A). The offline native client has no server or OpenCode
process, so its startup and memory are a renderer-only measurement; the report
labels the scope of every row and never presents them as a full-application
gain.

Receipts (raw samples, configuration and summaries) go to
``artifacts/native-bench-<time>/receipt.json``; artifacts/ is ignored by git.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import selectors
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts" / "parity"))
import virtual_display

DEFAULT_BINARY = REPO_ROOT / "native" / "target" / "release" / "openchamber-native"
MARK_PREFIX = "OPENCHAMBER_NATIVE_MARK "
CLOCK_TICKS = os.sysconf("SC_CLK_TCK")
PAGE_SIZE = os.sysconf("SC_PAGE_SIZE")


@dataclass
class NativeRun:
    """One native process from spawn to exit."""

    process: subprocess.Popen
    spawned_ns: int
    marks: list[dict] = field(default_factory=list)
    selector: selectors.BaseSelector = field(default_factory=selectors.DefaultSelector)
    buffer: str = ""

    def read_marks(self, timeout_s: float) -> None:
        """Collects marker lines that arrive within ``timeout_s``."""
        deadline = time.monotonic() + timeout_s
        while True:
            remaining = deadline - time.monotonic()
            ready = self.selector.select(max(0, remaining))
            if not ready:
                return
            for key, _ in ready:
                chunk = os.read(key.fd, 65536)
                if not chunk:
                    return
                self.buffer += chunk.decode("utf-8", "replace")
                *lines, self.buffer = self.buffer.split("\n")
                arrived_ms = (time.monotonic_ns() - self.spawned_ns) / 1e6
                for line in lines:
                    if line.startswith(MARK_PREFIX):
                        mark = json.loads(line[len(MARK_PREFIX):])
                        mark["arrivedMs"] = round(arrived_ms, 3)
                        self.marks.append(mark)

    def wait_for(self, marker: str, timeout_s: float, after: int = 0) -> dict:
        """Waits for the first ``marker`` at or after index ``after``."""
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            for mark in self.marks[after:]:
                if mark["marker"] == marker:
                    return mark
            if self.process.poll() is not None:
                raise RuntimeError(f"native process exited with {self.process.returncode} before {marker}")
            self.read_marks(min(0.05, max(0.0, deadline - time.monotonic())))
        raise TimeoutError(f"no {marker} marker within {timeout_s} s")


def apply_backend(env: dict[str, str], backend: str) -> dict[str, str]:
    """Pins both clients to the benchmark's owned Xvfb display."""
    if backend != "x11":
        raise ValueError("native benchmarks require the owned X11 display")
    return virtual_display.child_environment(env)


def native_env(args: argparse.Namespace) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith(("OPENCHAMBER_", "OPENCODE_"))}
    env = apply_backend(env, args.backend)
    home = args.native_home
    env.update(
        HOME=str(home),
        XDG_CONFIG_HOME=str(home / ".config"),
        XDG_DATA_HOME=str(home / ".local/share"),
        XDG_CACHE_HOME=str(home / ".cache"),
        XDG_STATE_HOME=str(home / ".local/state"),
    )
    env.update(
        OPENCHAMBER_NATIVE_DIAGNOSTICS="1",
        OPENCHAMBER_NATIVE_WORKLOAD=args.workload,
        OPENCHAMBER_NATIVE_THEME=args.theme,
        OPENCHAMBER_NATIVE_LOCALE="en",
        OPENCHAMBER_NATIVE_LOG="warn",
    )
    if args.server:
        env["OPENCHAMBER_NATIVE_SERVER"] = args.server
    return env


def launch_native(args: argparse.Namespace) -> NativeRun:
    spawned_ns = time.monotonic_ns()
    process = subprocess.Popen(
        [str(args.binary)],
        env=native_env(args),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    run = NativeRun(process=process, spawned_ns=spawned_ns)
    run.selector.register(process.stdout, selectors.EVENT_READ)
    return run


def is_running(pid: int) -> bool:
    """Whether a process exists and is not a zombie awaiting its parent."""
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
    except OSError:
        return False
    return state != "Z"


def stop_tree(process: subprocess.Popen, timeout_s: float = 10.0) -> dict:
    """Stops a process group this script started, reaps it and reports leftovers."""
    pid = process.pid
    owned = descendants(pid) | {pid}
    try:
        os.killpg(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        pass
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline and any(is_running(owned_pid) for owned_pid in owned):
        time.sleep(0.05)
    leftovers = sorted(owned_pid for owned_pid in owned if is_running(owned_pid))
    for owned_pid in leftovers:
        try:
            os.kill(owned_pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if leftovers:
        process.wait(timeout=timeout_s)
    return {"owned": len(owned), "leftAfterTerm": leftovers, "exitCode": process.returncode}


def descendants(pid: int) -> set[int]:
    children: dict[int, list[int]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
        except OSError:
            continue
        parent = int(stat.rsplit(")", 1)[1].split()[1])
        children.setdefault(parent, []).append(int(entry.name))
    found: set[int] = set()
    pending = [pid]
    while pending:
        for child in children.get(pending.pop(), []):
            if child not in found:
                found.add(child)
                pending.append(child)
    return found


def process_name(pid: int) -> str:
    """The executable name, with a Chromium child's role, such as ``openchamber:renderer``."""
    try:
        name = Path(f"/proc/{pid}/comm").read_text().strip()
        arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
    except OSError:
        return "?"
    for argument in arguments:
        if argument.startswith(b"--type="):
            return f"{name}:{argument[len(b'--type='):].decode('utf-8', 'replace')}"
    return name


def tree_memory(pid: int) -> dict:
    """Sums proportional, private and resident bytes over the process tree."""
    totals = {"pssBytes": 0, "privateBytes": 0, "rssBytes": 0, "processes": []}
    for owned_pid in sorted(descendants(pid) | {pid}):
        try:
            rollup = Path(f"/proc/{owned_pid}/smaps_rollup").read_text()
        except OSError:
            continue
        fields = {}
        for line in rollup.splitlines()[1:]:
            name, _, rest = line.partition(":")
            parts = rest.split()
            if parts and parts[0].isdigit():
                fields[name.strip()] = int(parts[0]) * 1024
        private = fields.get("Private_Clean", 0) + fields.get("Private_Dirty", 0)
        totals["pssBytes"] += fields.get("Pss", 0)
        totals["privateBytes"] += private
        totals["rssBytes"] += fields.get("Rss", 0)
        totals["processes"].append({"pid": owned_pid, "name": process_name(owned_pid), "pssBytes": fields.get("Pss", 0), "privateBytes": private})
    return totals


def tree_cpu_ticks(pid: int) -> int:
    ticks = 0
    for owned_pid in descendants(pid) | {pid}:
        try:
            fields = Path(f"/proc/{owned_pid}/stat").read_text().rsplit(")", 1)[1].split()
        except OSError:
            continue
        ticks += int(fields[11]) + int(fields[12])
    return ticks


def summarize(values: list[float]) -> dict | None:
    if not values:
        return None
    ordered = sorted(values)
    p95_index = max(0, min(len(ordered) - 1, round(0.95 * (len(ordered) - 1))))
    return {
        "n": len(ordered),
        "median": round(statistics.median(ordered), 3),
        "p95": round(ordered[p95_index], 3),
        "min": round(ordered[0], 3),
        "max": round(ordered[-1], 3),
    }


def complete_samples(samples: list[dict]) -> list[dict]:
    """A failed launch, including warmup, invalidates the group's statistics."""
    if any("error" in sample for sample in samples):
        return []
    return [sample for sample in samples if not sample.get("warmup", False)]


def native_startup_once(args: argparse.Namespace) -> dict:
    run = launch_native(args)
    sample: dict = {"client": "native", "selectionState": "unselected"}
    try:
        usable = run.wait_for("composer-usable", args.timeout)
        sample["composerUsableSinceSpawnMs"] = usable["arrivedMs"]
        sample["composerUsableSinceMainMs"] = usable["sinceStartMs"]
        if args.server:
            # Fresh profiles start without a selected session. History belongs
            # to the explicit selection setup in the switch scenario.
            sessions = run.wait_for("sessions-shown", args.timeout)
            sample["sessionsShownSinceSpawnMs"] = sessions["arrivedMs"]
            sample["sessionCount"] = sessions.get("count")
        run.read_marks(args.settle)
        sample["memoryAfterStartup"] = tree_memory(run.process.pid)
        if args.server_pid:
            sample["serverMemory"] = tree_memory(args.server_pid)
        before = tree_cpu_ticks(run.process.pid)
        time.sleep(args.idle_seconds)
        after = tree_cpu_ticks(run.process.pid)
        sample["idleCpuPercent"] = round((after - before) / CLOCK_TICKS / args.idle_seconds * 100, 3)
    except (TimeoutError, RuntimeError) as error:
        sample["error"] = str(error)
    finally:
        sample["marks"] = run.marks
        sample["cleanup"] = stop_tree(run.process)
        run.selector.close()
        run.process.stdout.close()
    return sample


def atspi_available() -> bool:
    try:
        import gi

        gi.require_version("Atspi", "2.0")
        from gi.repository import Atspi  # noqa: F401
    except (ImportError, ValueError):
        return False
    return shutil.which("busctl") is not None


def a11y_enabled() -> bool:
    output = subprocess.run(
        ["busctl", "--user", "get-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return output.strip().endswith("true")


def set_a11y(enabled: bool) -> None:
    subprocess.run(
        ["busctl", "--user", "set-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled", "b", "true" if enabled else "false"],
        check=True,
    )


def session_rows(node: object, depth: int = 0) -> dict[str, object]:
    """Session identities in tree order; project/worktree rows cannot be clicked."""
    if node is None:
        return {}
    if depth > 24:
        raise RuntimeError("session accessibility tree exceeds traversal limit")
    rows = {}
    key = node.get_accessible_id() or ""
    if node.get_role_name() == "tree item" and ":session:" in key:
        session_id = key.split(":session:", 1)[1]
        if session_id:
            rows[session_id] = node
    for index in range(node.get_child_count()):
        for session_id, row in session_rows(node.get_child_at_index(index), depth + 1).items():
            rows.setdefault(session_id, row)
    return rows


def switch_plan(rows: dict, switches: int, requested: list[str] | None) -> list[str]:
    """One explicit setup session followed by exactly the requested workload."""
    plan = requested if requested is not None else list(rows)[:switches + 1]
    if len(plan) != switches + 1 or len(set(plan)) != len(plan):
        raise RuntimeError(f"need {switches + 1} distinct session IDs including setup")
    if any(session_id not in rows for session_id in plan):
        raise RuntimeError("a planned session is absent from the sidebar")
    return plan


def native_switch_once(args: argparse.Namespace) -> dict:
    """Clicks sidebar rows through AT-SPI and reads the switch markers."""
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi

    def find_app(pid: int) -> object:
        # Match by process: another copy of the app can be on the bus under the same name.
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            desktop = Atspi.get_desktop(0)
            for index in range(desktop.get_child_count()):
                app = desktop.get_child_at_index(index)
                if app is not None and app.get_process_id() == pid:
                    return app
            time.sleep(0.1)
        raise TimeoutError("the native window never reached the AT-SPI bus")

    sample: dict = {"client": "native", "forward": [], "reverse": [],
                    "contentVerified": False,
                    "endCondition": "paint marker receipt; compositor presentation unverified"}
    run = launch_native(args)
    try:
        run.wait_for("composer-usable", args.timeout)
        if args.server:
            run.wait_for("sessions-shown", args.timeout)
        app = find_app(run.process.pid)
        deadline = time.monotonic() + args.timeout
        while True:
            rows = session_rows(app)
            try:
                plan = switch_plan(rows, args.switches, args.sessions)
                break
            except RuntimeError:
                if time.monotonic() >= deadline:
                    raise
                run.read_marks(min(0.05, max(0, deadline - time.monotonic())))
        sample["sessionIds"] = plan
        visited = set()

        def select(session_id: str) -> dict:
            # Drain previously emitted marks before opening this input boundary.
            run.read_marks(0)
            index = len(run.marks)
            row = session_rows(app)[session_id]
            row.clear_cache()
            if row.get_state_set().contains(Atspi.StateType.SELECTED):
                raise RuntimeError("planned session is already selected")
            action = row.get_action_iface()
            names = [action.get_action_name(index) for index in range(action.get_n_actions())]
            if "click" not in names:
                raise RuntimeError("session row did not expose a click action")
            action_index = names.index("click")
            deadline = time.monotonic() + args.timeout
            clicked_ns = time.monotonic_ns()
            if not action.do_action(action_index):
                raise RuntimeError("session row did not accept its click action")
            mark = run.wait_for("switch-completed", max(0, deadline - time.monotonic()), after=index)
            history = None
            if args.server and session_id not in visited:
                history = run.wait_for("history-shown", max(0, deadline - time.monotonic()), after=index)
            row = session_rows(app)[session_id]
            row.clear_cache()
            if not row.get_state_set().contains(Atspi.StateType.SELECTED):
                raise RuntimeError("completed switch did not select the planned session")
            visited.add(session_id)
            end = max([mark, history] if history else [mark], key=lambda item: item["arrivedMs"])
            elapsed_ms = end["arrivedMs"] - (clicked_ns - run.spawned_ns) / 1e6
            if elapsed_ms < 0:
                raise RuntimeError("switch endpoint predates its input")
            if mark.get("durationMs") is None:
                raise RuntimeError("switch marker has no selection duration")
            return {"sessionId": session_id, "rowKey": row.get_accessible_id(),
                    "selectionToPaintMs": mark.get("durationMs"),
                    "clickToMarkMs": round(elapsed_ms, 3),
                    "historyReadMs": history.get("durationMs") if history else None,
                    "cold": history is not None}

        sample["setup"] = select(plan[0])
        sample["selectionState"] = "workload-selected"
        forward = plan[1:]
        reverse = list(reversed(plan[:-1]))
        for order, sequence in (("forward", forward), ("reverse", reverse)):
            for session_id in sequence:
                sample[order].append(select(session_id))
        sample["memoryAfterSwitches"] = tree_memory(run.process.pid)
    except (TimeoutError, RuntimeError, ValueError, KeyError) as error:
        sample["error"] = str(error)
    finally:
        sample["marks"] = run.marks
        sample["cleanup"] = stop_tree(run.process)
        run.selector.close()
        run.process.stdout.close()
    return sample


def electron_env(home: Path, args: argparse.Namespace) -> dict[str, str]:
    """The isolation of ``scripts/profile-startup.mjs``: its own home, data and
    Electron profile, without the developer's OpenChamber, OpenCode or
    Electron variables, so the run never touches an installed app."""
    env = {key: value for key, value in os.environ.items() if not key.startswith(("OPENCHAMBER_", "OPENCODE_", "ELECTRON_")) and key != "NODE_OPTIONS"}
    env.update(
        HOME=str(home),
        SHELL="/bin/zsh",
        XDG_CONFIG_HOME=str(home / ".config"),
        XDG_DATA_HOME=str(home / ".local" / "share"),
        XDG_CACHE_HOME=str(home / ".cache"),
        XDG_STATE_HOME=str(home / ".local" / "state"),
        OPENCHAMBER_DATA_DIR=str(home / ".config" / "openchamber"),
        OPENCHAMBER_DESKTOP_USER_DATA_DIR=str(home / "userData"),
    )
    return apply_backend(env, args.backend)


def electron_memory_once(args: argparse.Namespace, home: Path) -> dict:
    """Samples the Electron process tree after the same settle as native."""
    spawned_ns = time.monotonic_ns()
    process = subprocess.Popen(
        [str(args.electron_app), "--ozone-platform=x11", "--disable-features=MacAppCodeSignClone", "--password-store=basic"],
        env=electron_env(home, args),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    sample: dict = {"client": "electron"}
    try:
        time.sleep(args.electron_memory_delay)
        if process.poll() is not None:
            raise RuntimeError(f"electron exited with {process.returncode}")
        sample["memoryAfterStartup"] = tree_memory(process.pid)
        before = tree_cpu_ticks(process.pid)
        time.sleep(args.idle_seconds)
        after = tree_cpu_ticks(process.pid)
        sample["idleCpuPercent"] = round((after - before) / CLOCK_TICKS / args.idle_seconds * 100, 3)
        sample["sampledSinceSpawnMs"] = round((time.monotonic_ns() - spawned_ns) / 1e6, 3)
    except RuntimeError as error:
        sample["error"] = str(error)
    finally:
        sample["cleanup"] = stop_tree(process)
    return sample


def electron_startup(args: argparse.Namespace, output: Path) -> dict:
    """Runs the existing packaged-desktop startup benchmark."""
    target = output / "electron-startup"
    command = [
        "node",
        str(REPO_ROOT / "scripts" / "profile-startup.mjs"),
        "--app",
        str(args.electron_app),
        "--runs",
        str(args.runs),
        "--warmup",
        str(args.warmup),
        "--output",
        str(target),
        "--opencode",
        args.electron_opencode,
    ]
    env = apply_backend(dict(os.environ), args.backend)
    completed = subprocess.run(command, capture_output=True, text=True, env=env)
    result: dict = {"command": command, "exitCode": completed.returncode, "stdoutTail": completed.stdout[-4000:], "stderrTail": completed.stderr[-4000:]}
    summary_path = target / "startup-summary.json"
    if summary_path.exists():
        result["summary"] = json.loads(summary_path.read_text())
    return result


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_revision() -> str:
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, capture_output=True, text=True).stdout.strip()


def stat_line(label: str, stats: dict | None, unit: str, scope: str) -> str:
    if stats is None:
        return f"| {label} | not run | | | | {scope} |"
    return f"| {label} | {stats['median']} {unit} | {stats['p95']} {unit} | {stats['min']}…{stats['max']} | {stats['n']} | {scope} |"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY, help="native release executable")
    parser.add_argument("--scenarios", default="startup,switch", help="comma-separated: startup (includes memory and idle), switch")
    parser.add_argument("--runs", type=int, default=5, help="measured launches per client")
    parser.add_argument("--warmup", type=int, default=1, help="discarded launches per client")
    parser.add_argument("--workload", default="representative", choices=["representative", "stress", "small"])
    parser.add_argument("--theme", default="openchamber-dark")
    parser.add_argument("--backend", default="x11", choices=["x11"], help="owned Xvfb display for both clients")
    parser.add_argument("--switches", type=int, default=10, help="rows visited per switch order")
    parser.add_argument("--sessions", type=lambda value: value.split(","), help="ordered session IDs, setup first, then --switches targets")
    parser.add_argument("--settle", type=float, default=1.5, help="seconds after usable before sampling memory")
    parser.add_argument("--idle-seconds", type=float, default=5.0)
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--electron-app", type=Path, help="packaged Electron executable for separate reference diagnostics")
    parser.add_argument("--electron-opencode", default="cold", choices=["warm", "cold"], help="cold: Electron starts its own OpenCode, as a user launch does")
    parser.add_argument("--electron-memory-delay", type=float, default=20.0, help="seconds after spawn before sampling Electron memory")
    parser.add_argument("--server", help="OpenChamber server URL: the native client runs connected (an isolated server only)")
    parser.add_argument("--server-pid", type=int, help="the server's process, sampled for the full-application memory")
    parser.add_argument("--output", type=Path, help="receipt directory")
    args = parser.parse_args()
    scenarios = {scenario.strip() for scenario in args.scenarios.split(",") if scenario.strip()}
    if args.switches < 1:
        parser.error("--switches must be positive")
    if args.server and "switch" in scenarios and not args.sessions:
        parser.error("connected switches require --sessions with setup and target identities")
    if args.sessions and (len(args.sessions) != args.switches + 1 or len(set(args.sessions)) != len(args.sessions) or not all(args.sessions)):
        parser.error("--sessions must contain exactly --switches + 1 distinct nonempty IDs")

    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            print(f"no private display: {problem}", file=sys.stderr)
            return 2
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], 1280, 800)

    if not args.binary.exists():
        print(f"native binary not found: {args.binary}; run bun run native:build", file=sys.stderr)
        return 2
    output = args.output or REPO_ROOT / "artifacts" / f"native-bench-{time.strftime('%Y%m%dT%H%M%S')}"
    output.mkdir(parents=True, exist_ok=True)
    scratch = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    native_profile = tempfile.TemporaryDirectory(prefix="native-bench-profile-", dir=scratch)
    args.native_home = Path(native_profile.name)
    receipt: dict = {
        "generatedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "revision": git_revision(),
        "host": {"platform": platform.platform(), "machine": platform.machine(), "cpus": os.cpu_count(), "session": os.environ.get("XDG_SESSION_TYPE"), "backend": args.backend},
        "native": {"binary": str(args.binary), "sha256": file_sha256(args.binary), "workload": args.workload, "theme": args.theme, "server": args.server},
        "electron": {"app": str(args.electron_app) if args.electron_app else None},
        "configuration": {key: str(value) for key, value in vars(args).items()},
        "samples": {"nativeStartup": [], "electronMemory": [], "nativeSwitch": []},
        "comparison": {"eligible": False, "reasons": [
            "Electron reference does not replay the native ordered session workload",
            "ordered transcript content and compositor presentation are unverified",
            "Electron memory uses a fixed delay, native memory follows readiness",
        ]},
    }

    electron_home = Path(tempfile.mkdtemp(prefix="openchamber-native-bench-electron-")) if args.electron_app else None
    try:
        if "startup" in scenarios:
            for run in range(1 - args.warmup, args.runs + 1):
                # A/B on odd runs and B/A on even ones balances drift between clients.
                order = ["native", "electron"] if run % 2 else ["electron", "native"]
                for client in order:
                    if client == "native":
                        sample = native_startup_once(args)
                    elif electron_home is not None:
                        sample = electron_memory_once(args, electron_home)
                    else:
                        continue
                    sample["run"] = run
                    sample["warmup"] = run < 1
                    key = "nativeStartup" if client == "native" else "electronMemory"
                    receipt["samples"][key].append(sample)
                    print(f"{'warm-up' if run < 1 else f'run {run}'} {client}: {sample.get('error') or 'ok'}", flush=True)
            if args.electron_app:
                receipt["electron"]["startup"] = electron_startup(args, output)

        if "switch" in scenarios:
            if not atspi_available():
                receipt["switchSkipped"] = "PyGObject AT-SPI or busctl unavailable"
            else:
                previous = a11y_enabled()
                set_a11y(True)
                try:
                    for run in range(1, args.runs + 1):
                        sample = native_switch_once(args)
                        sample["run"] = run
                        receipt["samples"]["nativeSwitch"].append(sample)
                        print(f"switch run {run}: {sample.get('error') or 'ok'}", flush=True)
                finally:
                    set_a11y(previous)
                receipt["a11yRestoredTo"] = previous
    finally:
        native_profile.cleanup()
        if electron_home is not None:
            shutil.rmtree(electron_home, ignore_errors=True)

    measured = complete_samples(receipt["samples"]["nativeStartup"])
    electron_measured = complete_samples(receipt["samples"]["electronMemory"])
    switches = complete_samples(receipt["samples"]["nativeSwitch"])
    electron_summary = (receipt["electron"].get("startup") or {}).get("summary", {})
    electron_startup_result = receipt["electron"].get("startup") or {}
    electron_usable = (((electron_summary.get("summaries") or {}).get("A") or {}).get("usable")
                       if electron_startup_result.get("exitCode") == 0 else None)
    def pss_by_process(samples: list[dict]) -> dict:
        """Per-role PSS: same-named processes of one sample add up first."""
        per_sample: list[dict[str, int]] = []
        for sample in samples:
            roles: dict[str, int] = {}
            for process in sample["memoryAfterStartup"]["processes"]:
                roles[process["name"]] = roles.get(process["name"], 0) + process["pssBytes"]
            per_sample.append(roles)
        names = sorted({name for roles in per_sample for name in roles})
        return {name: summarize([roles.get(name, 0) for roles in per_sample]) for name in names}

    summary = {
        "nativeComposerUsableSinceSpawnMs": summarize([s["composerUsableSinceSpawnMs"] for s in measured]),
        "nativeSessionsShownSinceSpawnMs": summarize([s["sessionsShownSinceSpawnMs"] for s in measured if "sessionsShownSinceSpawnMs" in s]),
        "serverPssBytes": summarize([s["serverMemory"]["pssBytes"] for s in measured if "serverMemory" in s]),
        "nativeWithServerPssBytes": summarize([s["memoryAfterStartup"]["pssBytes"] + s["serverMemory"]["pssBytes"] for s in measured if "serverMemory" in s]),
        "nativePssBytes": summarize([s["memoryAfterStartup"]["pssBytes"] for s in measured]),
        "nativePrivateBytes": summarize([s["memoryAfterStartup"]["privateBytes"] for s in measured]),
        "nativeIdleCpuPercent": summarize([s["idleCpuPercent"] for s in measured]),
        "nativeSwitchForwardMs": summarize([e["clickToMarkMs"] for s in switches for e in s["forward"]]),
        "nativeSwitchReverseMs": summarize([e["clickToMarkMs"] for s in switches for e in s["reverse"]]),
        "nativePssAfterSwitchesBytes": summarize([s["memoryAfterSwitches"]["pssBytes"] for s in switches if "memoryAfterSwitches" in s]),
        "electronComposerUsableSinceSpawnMs": electron_usable,
        "electronPssBytes": summarize([s["memoryAfterStartup"]["pssBytes"] for s in electron_measured]),
        "electronPrivateBytes": summarize([s["memoryAfterStartup"]["privateBytes"] for s in electron_measured]),
        "electronIdleCpuPercent": summarize([s["idleCpuPercent"] for s in electron_measured]),
        "electronPssByProcess": pss_by_process(electron_measured),
        "failures": (sum(1 for group in receipt["samples"].values() for sample in group if "error" in sample)
                     + int(bool(receipt.get("switchSkipped")))
                     + int(bool(electron_startup_result.get("exitCode")))),
        "leftoverProcesses": sum(len(sample["cleanup"]["leftAfterTerm"]) for group in receipt["samples"].values() for sample in group),
    }
    receipt["summary"] = summary
    (output / "receipt.json").write_text(json.dumps(receipt, indent=2))

    def mib_stats(stats: dict | None) -> dict | None:
        return None if stats is None else {key: (round(value / (1 << 20), 1) if key != "n" else value) for key, value in stats.items()}

    native_scope = (
        "native connected: window and client; the server and OpenCode are outside this tree"
        if args.server
        else "native offline: window, synthetic data, no server or OpenCode"
    )
    electron_scope = "Electron packaged: window, in-process server, OpenCode child"
    print("\n| Metric | Median | p95 | Range | n | Scope |")
    print("| --- | --- | --- | --- | --- | --- |")
    print(stat_line("Native composer usable since spawn", summary["nativeComposerUsableSinceSpawnMs"], "ms", native_scope))
    print(stat_line("Electron composer usable since spawn", summary["electronComposerUsableSinceSpawnMs"], "ms", electron_scope + f", opencode {args.electron_opencode}"))
    if args.server:
        print(stat_line("Native sessions shown since spawn", summary["nativeSessionsShownSinceSpawnMs"], "ms", native_scope))
    print(stat_line("Native PSS after startup", mib_stats(summary["nativePssBytes"]), "MiB", native_scope))
    if summary["serverPssBytes"]:
        print(stat_line("Server PSS (server and OpenCode)", mib_stats(summary["serverPssBytes"]), "MiB", "the isolated server's process tree"))
        print(stat_line("Native and server PSS", mib_stats(summary["nativeWithServerPssBytes"]), "MiB", "combined ownership; workloads are not matched"))
    print(stat_line("Electron PSS after startup", mib_stats(summary["electronPssBytes"]), "MiB", electron_scope))
    print(stat_line("Native private after startup", mib_stats(summary["nativePrivateBytes"]), "MiB", native_scope))
    print(stat_line("Electron private after startup", mib_stats(summary["electronPrivateBytes"]), "MiB", electron_scope))
    print(stat_line("Native idle CPU", summary["nativeIdleCpuPercent"], "%", native_scope))
    print(stat_line("Electron idle CPU", summary["electronIdleCpuPercent"], "%", electron_scope))
    print(stat_line("Native switch, forward", summary["nativeSwitchForwardMs"], "ms", "AT-SPI dispatch to paint marker receipt; cold visits include history"))
    print(stat_line("Native switch, reverse", summary["nativeSwitchReverseMs"], "ms", "AT-SPI dispatch to paint marker receipt; cached visits"))
    print(stat_line("Native PSS after switches", mib_stats(summary["nativePssAfterSwitchesBytes"]), "MiB", native_scope))
    for name, stats in summary["electronPssByProcess"].items():
        print(stat_line(f"Electron PSS, {name}", mib_stats(stats), "MiB", "per-process share of the Electron tree"))
    if receipt.get("switchSkipped"):
        print(f"\nswitch scenario not run: {receipt['switchSkipped']}")
    print(f"\nfailures: {summary['failures']}, processes left after SIGTERM: {summary['leftoverProcesses']}")
    print("Cross-client comparison unavailable: " + "; ".join(receipt["comparison"]["reasons"]))
    print(f"receipt: {output / 'receipt.json'}")
    return 1 if summary["failures"] else 0


if __name__ == "__main__":
    sys.exit(main())
