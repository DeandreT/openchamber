"""Real-window permission shield qualification on a fresh local fixture.

Run: python3 scripts/parity/native_permission_modes.py --scale 1.25
The installed candidate must implement the shield. A legacy binary fails.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import sys
import time
import traceback
import urllib.parse
import uuid

from native_adapter import accessibility_enabled, set_accessibility
from native_interactions import Fixture, Interaction, Interrupts, RunInterrupted, until
from native_platform import binary_info
from native_selections import require_clean_result
import runner
import virtual_display

LABELS = {
    "ask": "Permissions: ask every time",
    "safety": "Permissions: safety net, ask only before risky actions",
    "auto": "Permissions: accept everything",
}
# The engine computes routing on first read, which can exceed the fixture's
# default read timeout on a cold start.
ROUTING_TIMEOUT = 60


def safety_offered(routing):
    """Safety is offered only when routing is available and JEV is not ruled out."""
    return routing.get("available") is True and routing.get("jevAvailable", True) is not False


def expected_cycle(safety):
    """Two full turns of the shield from Ask, ending on Auto."""
    turn = (["safety"] if safety else []) + ["auto", "ask"]
    return turn + turn[:-1]


def server_mode(policy, session_id):
    """The mode the server reports for one session, as the native client reads it."""
    modes = policy.get("modes")
    if isinstance(modes, dict) and modes.get(session_id) in LABELS:
        return modes[session_id]
    sessions = policy.get("sessions")
    if isinstance(sessions, dict) and isinstance(sessions.get(session_id), bool):
        return "auto" if sessions[session_id] else "ask"
    return "ask"


def verify_steps(steps, cycle):
    """Each press must reach the server and the shield, with a newer revision."""
    if [step["expected"] for step in steps] != cycle:
        raise ValueError("shield presses did not follow the expected cycle")
    previous = None
    for step in steps:
        if step["label"] != LABELS[step["expected"]] or step["serverMode"] != step["expected"]:
            raise ValueError(f"shield and server disagree after {step['expected']}")
        after = step["revisionAfter"]
        if not isinstance(after, int) or isinstance(after, bool):
            raise ValueError("server policy has no revision")
        if previous is not None and after <= previous:
            raise ValueError("server revision did not advance")
        previous = after


class Modes(Interaction):
    def policy(self):
        return self.fixture.get("/api/permission-auto-accept")

    def mode(self):
        return server_mode(self.policy(), self.raw_id)

    def shield(self):
        return self.client._find("composer-permissions").get_name()

    def reopen(self, label):
        self.click("window-close")
        self.client.process.wait(timeout=15)
        if self.client.process.returncode != 0:
            raise RuntimeError("native window close failed")
        self.start(label)

    def press(self, expected, steps):
        before = self.policy().get("revision")
        self.click("composer-permissions")
        until(f"server did not record {expected}", lambda: self.mode() == expected, self.args.timeout)
        until(f"shield did not show {expected}", lambda: self.shield() == LABELS[expected], self.args.timeout)
        after = self.policy()
        steps.append({"expected": expected, "label": self.shield(), "serverMode": server_mode(after, self.raw_id),
                      "revisionBefore": before, "revisionAfter": after.get("revision")})
        self.client.screenshot(self.fixture.evidence / f"shield-{len(steps)}-{expected}.png")

    def run(self):
        routing = self.fixture.get("/api/routing", timeout=ROUTING_TIMEOUT)
        safety = safety_offered(routing)
        session = self.fixture.create_session("interaction-shell")
        self.raw_id = session["id"]
        self.session_id = urllib.parse.quote(session["id"], safe="")
        self.start("first")
        initial = {"label": self.shield(), "serverMode": self.mode()}
        if initial != {"label": LABELS["ask"], "serverMode": "ask"}:
            raise RuntimeError(f"unexpected initial shield: {initial}")
        cycle = expected_cycle(safety)
        steps = []
        for expected in cycle:
            self.press(expected, steps)
        verify_steps(steps, cycle)
        self.reopen("restart")
        until("mode did not survive restart", lambda: self.shield() == LABELS["auto"], self.args.timeout)
        restart = {"label": self.shield(), "serverMode": self.mode()}
        self.client.screenshot(self.fixture.evidence / "after-restart.png")

        # A shell turn in Auto: the server answers; the window never shows a card.
        dock_seen = []
        self.send()

        def finished():
            if any((node.get_accessible_id() or "") == "request-dock" for node in self.client._walk()):
                dock_seen.append(time.monotonic())
            return self.settled() and any(item.get("type") == "assistant" and item.get("time", {}).get("completed")
                                          for item in self.messages())
        until("auto turn did not settle", finished, self.args.timeout * 2)
        self.shell_done(1)
        if dock_seen:
            raise RuntimeError("a permission card was shown in an Auto session")
        if self.pending("permission"):
            raise RuntimeError("a permission is still pending in an Auto session")
        self.client.screenshot(self.fixture.evidence / "auto-turn.png")
        return {"sessionID": self.raw_id,
                "routing": {"available": routing.get("available"), "jevAvailable": routing.get("jevAvailable")},
                "safetyOffered": safety, "initial": initial, "steps": steps, "afterRestart": restart,
                "autoTurn": {"shellCompleted": True, "dockShown": False}}


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    inputs = {"native": identity, "opencode": binary_info(args.opencode_binary),
              "dist": runner.artifact_identity(args.dist), "script": binary_info(Path(__file__).resolve()),
              "fixtureScript": binary_info(runner.ISOLATED_SERVER),
              "providerScript": binary_info(runner.ISOLATED_SERVER.with_name("fixture-provider.mjs"))}
    for name in ("native_interactions.py", "native_selections.py", "native_adapter.py", "virtual_display.py"):
        inputs[name] = binary_info(Path(__file__).with_name(name).resolve())
    result = {"passed": False, "status": "failed", "display": "owned-xvfb", "scale": args.scale, "inputs": inputs}
    evidence = args.output / "permission-modes"
    previous = accessibility_enabled()
    interaction = None
    try:
        with Interrupts() as interrupts:
            set_accessibility(True)
            root = Path("/tmp/opencode") / f"native-permission-modes-fixture-{uuid.uuid4().hex}"
            result["fixtureRoot"] = str(root)
            with Fixture(root, evidence, args, interrupts) as fixture, ExitStack() as cleanup:
                interaction = Modes(fixture, args, identity, cleanup)
                try:
                    result["outcomes"] = interaction.run()
                except BaseException:
                    if interaction.client and interaction.client.process and interaction.client.process.poll() is None:
                        try:
                            interaction.client.screenshot(evidence / "failed.png")
                        except Exception as error:
                            result["screenshotError"] = str(error)
                    raise
            require_clean_result(interaction.clients)
            result.update(passed=True, status="passed")
    except (Exception, KeyboardInterrupt) as error:
        result["error"] = str(error)
        result["interrupted"] = isinstance(error, (RunInterrupted, KeyboardInterrupt))
        (args.output / "failure.txt").write_text(traceback.format_exc())
    finally:
        try:
            set_accessibility(previous)
            for name, item in inputs.items():
                current = runner.artifact_identity(Path(item["path"])) if name == "dist" else binary_info(Path(item["path"]))
                if current != item:
                    raise RuntimeError(f"qualification input changed: {name}")
        except Exception as error:
            result.update(passed=False, status="failed", finalizationError=str(error))
        if interaction:
            result["nativeProcesses"] = [{"pid": client.process.pid, "exitCode": client.process.poll()}
                                         for client in interaction.clients if client.process is not None]
            result["inputSubstitutions"] = [item for client in interaction.clients for item in client.substitutions]
        (args.output / "report.json").write_text(json.dumps(result, indent=2, ensure_ascii=False))
        print(f"{result['status']}: {args.output / 'report.json'}", flush=True)
    return 0 if result["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    for name in ("binary", "opencode_binary", "dist"):
        setattr(args, name, getattr(args, name).resolve())
    for path in (args.binary, args.opencode_binary):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"missing executable: {path}")
    args.output = (args.output or Path("/tmp/opencode") / f"native-permission-modes-{uuid.uuid4().hex}").absolute()
    if args.output.parent != Path("/tmp/opencode") or args.output.parent.resolve() != Path("/tmp/opencode") or args.output.exists():
        parser.error("--output must be a new directory directly under /tmp/opencode")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if not virtual_display.inside():
        if problem := virtual_display.available():
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
