#!/usr/bin/env python3
"""Native transcript rows on an owned display: tool rows, reasoning blocks,
compaction rows and left-out tool results.

Starts an isolated server with the fixture provider, seeds one session per
feature model (`seed-long-session.mjs`), opens each in the native client and
checks what the rows expose and do. A tool result the history page left out
must load when its row opens. Screenshots of every state go to the output
directory. Native only: the Electron side of these rows needs the runner's
paired steps (scroll, hover, clipboard) first.

    python3 scripts/parity/native_transcript.py --binary <native> \\
        --repo <checkout with a built server> --opencode-binary <opencode>
"""
from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import time
import traceback
import urllib.request
from contextlib import ExitStack
from pathlib import Path
from urllib.parse import quote

sys.path.insert(0, str(Path(__file__).resolve().parent))

import runner  # noqa: E402
import virtual_display  # noqa: E402
from native_adapter import NativeClient, accessibility_enabled, set_accessibility  # noqa: E402

THEME = "openchamber-dark"
# Title, models and seed options for each session the checks open.
SESSIONS = [
    ("transcript: reasoning", "perf/reasoning-20000cps", []),
    ("transcript: failing tool", "perf/failtool-20000cps", []),
    ("transcript: diff", "perf/diff-20000cps", []),
    ("transcript: rich", "perf/rich-20000cps", ["--image"]),
    ("transcript: compaction", "perf/stream-20000cps", ["--compact"]),
]


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class Server:
    """`isolated-server.mjs` on a root this run owns."""

    def __init__(self, args, root: Path):
        self.port = free_port()
        self.log = (root / "isolated-server.log").open("w")
        self.process = subprocess.Popen(
            ["node", str(runner.ISOLATED_SERVER), "--repo", str(args.repo), "--root", str(root),
             "--port", str(self.port), "--fixture-port", str(free_port()),
             "--opencode-binary", str(args.opencode_binary)],
            cwd=runner.REPO_ROOT, stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True,
        )
        self.project = root / "project"

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def wait_ready(self, timeout_s: float) -> None:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError("the isolated server exited")
            try:
                with urllib.request.urlopen(f"{self.url}/health", timeout=2) as response:
                    if json.load(response).get("isOpenCodeReady"):
                        return
            except OSError:
                pass
            time.sleep(0.25)
        raise TimeoutError("the isolated server never became ready")

    def stop(self) -> None:
        try:
            os.killpg(self.process.pid, 2)
            self.process.wait(timeout=30)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            os.killpg(self.process.pid, 9)
        self.log.close()

    def messages(self, session: str) -> list[dict]:
        with urllib.request.urlopen(f"{self.url}/api/session/{quote(session)}/message?limit=50", timeout=15) as response:
            document = json.load(response)
        return document.get("items") or document.get("data") or []


def seed(server: Server) -> dict[str, str]:
    sessions = {}
    for title, model, options in SESSIONS:
        output = subprocess.run(
            ["node", str(runner.SEED_SESSION), "--port", str(server.port), "--dir", str(server.project),
             "--turns", "1", "--title", title, "--models", model, "--json", *options],
            cwd=runner.REPO_ROOT, check=True, capture_output=True, text=True,
        ).stdout
        sessions[title] = json.loads(output.strip().splitlines()[-1])["sessionId"]
    return sessions


def tool_calls(server: Server, session: str) -> list[dict]:
    calls = []
    for message in server.messages(session):
        info = message.get("info", message)
        for item in info.get("content") or []:
            if item.get("type") == "tool":
                calls.append(item)
    return calls


def ids(client: NativeClient) -> set[str]:
    return {node.get_accessible_id() or "" for node in client._walk()}


def node(client: NativeClient, accessible_id: str):
    return client._find(accessible_id)


def expanded(client: NativeClient, accessible_id: str) -> bool:
    from native_adapter import _atspi

    target = node(client, accessible_id)
    target.clear_cache()
    return target.get_state_set().contains(_atspi().StateType.EXPANDED)


def wait_until(predicate, timeout_s: float, what: str) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except LookupError:
            pass
        time.sleep(0.1)
    raise TimeoutError(f"{what} within {timeout_s} s")


def open_session(client: NativeClient, title: str) -> None:
    row = runner.resolve_row(client.rows(), {"title": title})
    client.click(row)
    wait_until(lambda: "transcript" in ids(client) and any(i.startswith("message:") for i in ids(client)),
               20, f"the {title!r} history")
    time.sleep(0.5)


def qualify(args) -> int:
    virtual_display.verify_owned_display()
    args.output.mkdir(parents=True)
    results = []
    previous = accessibility_enabled()
    clients: list[NativeClient] = []

    def check(name, run):
        try:
            run()
            results.append({"check": name, "passed": True})
        except (OSError, RuntimeError, TimeoutError, LookupError, ValueError, KeyError, AssertionError,
                subprocess.SubprocessError) as error:
            traceback.print_exc()
            results.append({"check": name, "passed": False, "error": str(error)})
            if clients:
                try:
                    clients[-1].screenshot(args.output / f"{name}-failed.png")
                    (args.output / f"{name}-ids.txt").write_text("\n".join(sorted(ids(clients[-1]))))
                except Exception:  # noqa: BLE001 - evidence is best effort after a failure
                    pass

    try:
        with ExitStack() as cleanup:
            root = args.output / "server"
            root.mkdir()
            server = Server(args, root)
            cleanup.callback(server.stop)
            server.wait_ready(90)
            sessions = seed(server)
            state = args.output / "client"
            for directory in runner.state_dirs(state).values():
                Path(directory).mkdir(parents=True, exist_ok=True)
            env = runner.native_env(state, server.url, THEME)
            set_accessibility(True)
            log = cleanup.enter_context((args.output / "native.log").open("w"))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(90, connected=True)
            clients.append(client)

            def failing_tool():
                open_session(client, "transcript: failing tool")
                call = next(call for call in tool_calls(server, sessions["transcript: failing tool"])
                            if call["state"]["status"] == "error")
                header = f"tool:{call['id']}:header"
                client.screenshot(args.output / "tool-failed-collapsed.png")
                assert not expanded(client, header), "a tool row starts folded"
                client.click(header)
                wait_until(lambda: expanded(client, header), 5, "the failed row to open")
                client.screenshot(args.output / "tool-failed-expanded.png")
                client.click(header)
                wait_until(lambda: not expanded(client, header), 5, "the failed row to fold")

            def reasoning():
                open_session(client, "transcript: reasoning")
                toggle = next(i for i in ids(client) if i.startswith("reasoning:") and i.endswith(":toggle"))
                assert not expanded(client, toggle), "finished reasoning starts folded"
                client.screenshot(args.output / "reasoning-collapsed.png")
                client.click(toggle)
                wait_until(lambda: expanded(client, toggle), 5, "the reasoning block to open")
                client.screenshot(args.output / "reasoning-expanded.png")

            def diff():
                open_session(client, "transcript: diff")
                edit = next(call for call in tool_calls(server, sessions["transcript: diff"]) if call["name"] == "edit")
                header = f"tool:{edit['id']}:header"
                client.click(header)
                wait_until(lambda: expanded(client, header), 5, "the edit row to open")
                time.sleep(0.5)
                client.screenshot(args.output / "tool-diff-expanded.png")

            def elided_result():
                open_session(client, "transcript: failing tool")
                call = next(call for call in tool_calls(server, sessions["transcript: failing tool"])
                            if call["name"] == "glob")
                header = f"tool:{call['id']}:header"
                client.click(header)
                wait_until(lambda: expanded(client, header), 5, "the glob row to open")
                client.screenshot(args.output / "tool-elided-opened.png")
                # The page left the result out; the whole message fills it in
                # and the row's copy action appears with the output.
                wait_until(lambda: f"tool:{call['id']}:copy" in ids(client), 15, "the left-out result to load")
                client.screenshot(args.output / "tool-elided-hydrated.png")

            def rich_content():
                open_session(client, "transcript: rich")
                present = ids(client)
                for name in ("inline.png", "gradient.png"):
                    assert f"attachment:{name}" in present, f"the {name} thumbnail"
                # Diagrams lay out off the main thread; give the first one a moment.
                time.sleep(1.5)
                client.screenshot(args.output / "rich-content.png")

            def compaction():
                open_session(client, "transcript: compaction")
                toggle = next(i for i in ids(client) if i.startswith("compaction:") and i.endswith(":toggle"))
                client.screenshot(args.output / "compaction-collapsed.png")
                client.click(toggle)
                wait_until(lambda: expanded(client, toggle), 5, "the compaction summary to open")
                client.screenshot(args.output / "compaction-expanded.png")

            for name, run in (("failing-tool", failing_tool), ("reasoning", reasoning), ("diff", diff),
                              ("elided-result", elided_result), ("rich-content", rich_content),
                              ("compaction", compaction)):
                if not args.only or name in args.only:
                    check(name, run)
    finally:
        set_accessibility(previous)
        (args.output / "report.json").write_text(json.dumps({
            "binary": str(args.binary), "display": "owned-xvfb", "scale": args.scale, "checks": results,
        }, indent=2))
    return 0 if results and all(result["passed"] for result in results) else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--repo", type=Path, default=runner.REPO_ROOT,
                        help="checkout whose built server runs (default: this one)")
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--only", action="append", help="run only the named check; repeatable")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    args.repo = args.repo.resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("the native binary must be executable")
    args.output = (args.output or Path("/tmp/opencode") / f"native-transcript-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.output.exists():
        parser.error("choose a new output directory")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
