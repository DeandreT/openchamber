"""Real-window session goal and review loop qualification on a fresh fixture.

Run: python3 scripts/parity/native_goals_review.py --scale 1.25
The fixture's workflow model works a goal over as many turns as its objective
asks and answers the goal's progress check from its report, so every state is
reached deterministically: armed, running, waiting (the progress check),
completed, stopped (paused), failed (blocked) and recovered after a restart. The
review loop runs handoff, review, fixes and re-review to a clean end, and a
second run is stopped.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import sys
import traceback
import urllib.parse
import urllib.request
import uuid

from native_adapter import _atspi, accessibility_enabled, set_accessibility
from native_interactions import Fixture, Interaction, Interrupts, RunInterrupted, until
from native_platform import binary_info
from native_selections import require_clean_result
import runner
import virtual_display

MODEL = "workflow-1s"
ARMED = "Goal armed — tap to disarm"
UNARMED = "Start a goal with the next message"
MANAGE = "Manage session goal"
# Two turns, each with the server's quiet window and progress check.
GOAL_TIMEOUT = 120


class WorkflowFixture(Fixture):
    """The same owned server and cleanup, with the workflow models."""

    MODE = "--workflows"

    def request(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.url + path, data=data, method=method, headers={
            "Content-Type": "application/json",
            "x-opencode-directory": urllib.parse.quote(str(self.project), safe=""),
        })
        with urllib.request.urlopen(request, timeout=10) as response:
            return json.load(response)


class Goals(Interaction):
    def goal(self):
        """The goal the server stores on the session, or None."""
        session = self.fixture.data(f"/api/session/{self.session_id}", dict)
        return (session.get("metadata") or {}).get("openchamber", {}).get("goal")

    def status(self):
        goal = self.goal()
        return goal.get("status") if goal else None

    def button(self):
        return self.client._find("composer-goal").get_name()

    def present(self, accessible_id):
        return any((node.get_accessible_id() or "") == accessible_id for node in self.client._walk())

    def type_and_send(self, text):
        composer = until("composer entry did not appear", self.composer, self.args.timeout)
        self.paste(composer, text)
        until("send control did not enable",
              lambda: self.client._find("composer-submit").get_state_set().contains(_atspi().StateType.ENABLED))
        self.client.key("Return")

    def shot(self, name, shots):
        self.client.screenshot(self.fixture.evidence / f"{len(shots) + 1:02d}-{name}.png")
        shots.append(name)

    def arm_and_send(self, objective, shots):
        until("goal button did not offer to arm", lambda: self.button() == UNARMED, self.args.timeout)
        self.click("composer-goal")
        until("goal button did not arm", lambda: self.button() == ARMED, self.args.timeout)
        self.shot("armed", shots)
        self.type_and_send(objective)
        until("the goal was not saved", lambda: self.status() == "active", self.args.timeout)
        until("the prompt was not admitted", lambda: any(
            message.get("type") == "user" and message.get("text") == objective for message in self.messages()),
            self.args.timeout)
        goal = self.goal()
        objective_file = self.fixture.get(f"/api/goals/objective/{self.session_id}").get("content")
        if not goal.get("objectiveFile") or objective_file != objective:
            raise RuntimeError("the objective did not land in its file")
        until("goal row did not appear", lambda: self.present("goal-row"), self.args.timeout)
        until("goal button did not turn into the manage control", lambda: self.button() == MANAGE, self.args.timeout)
        return goal

    def remove_goal(self, shots):
        self.click("composer-goal")
        self.click("goal-dialog-clear")
        until("goal was not removed", lambda: self.goal() is None, self.args.timeout)
        until("goal row stayed", lambda: not self.present("goal-row"), self.args.timeout)
        self.shot("removed", shots)

    def reopen(self, label):
        self.click("window-close")
        self.client.process.wait(timeout=15)
        if self.client.process.returncode != 0:
            raise RuntimeError("native window close failed")
        self.start(label)

    def run_goals(self, shots):
        outcomes = {}
        # Running, waiting, completed.
        first = self.arm_and_send("Native goal fixture [turns:2]", shots)
        self.shot("running", shots)
        until("the turn did not end", self.settled, GOAL_TIMEOUT)
        seen_waiting = []

        def completed():
            if self.status() == "active" and self.settled():
                seen_waiting.append(True)
            return self.status() == "complete"
        until("the goal did not complete", completed, GOAL_TIMEOUT)
        goal = self.goal()
        if goal.get("id") != first.get("id") or goal.get("turnsUsed", 0) < 1:
            raise RuntimeError("the completed goal is not the one the send started")
        until("goal row did not show completion", lambda: self.present("goal-row") and not self.present("goal-row-action"),
              self.args.timeout)
        self.shot("completed", shots)
        outcomes["completed"] = {"turnsUsed": goal.get("turnsUsed"), "waitingObserved": bool(seen_waiting),
                                 "evaluationModel": goal.get("evaluationModelID")}
        self.remove_goal(shots)

        # Stopped, resumed and recovered across a restart.
        self.arm_and_send("Native goal fixture to stop [turns:9]", shots)
        until("pause did not appear", lambda: self.present("goal-row-action"), self.args.timeout)
        self.click("goal-row-action")
        until("the goal did not pause", lambda: self.status() == "paused", self.args.timeout)
        until("the stopped turn did not settle", self.settled, GOAL_TIMEOUT)
        self.shot("stopped", shots)
        self.reopen("restart")
        until("goal row did not come back after a restart", lambda: self.present("goal-row"), self.args.timeout)
        until("resume did not come back after a restart", lambda: self.present("goal-row-action"), self.args.timeout)
        self.shot("recovered", shots)
        self.click("goal-row-action")
        until("the goal did not resume", lambda: self.status() == "active", self.args.timeout)
        resumed = self.goal()
        if resumed.get("statusReason") != "resumed" or resumed.get("turnsUsed") != 0:
            raise RuntimeError("Resume did not grant a fresh turn allowance")
        outcomes["stopped"] = {"paused": True, "recoveredAfterRestart": True, "resumed": True}
        self.click("goal-row-action")
        until("the goal did not pause again", lambda: self.status() == "paused", self.args.timeout)
        until("the turn did not settle", self.settled, GOAL_TIMEOUT)
        self.remove_goal(shots)

        # Failed: only the user can unblock it.
        self.arm_and_send("Native goal fixture that needs a key [blocked]", shots)
        until("the goal did not block", lambda: self.status() == "blocked", GOAL_TIMEOUT)
        until("blocked goal row did not offer Resume", lambda: self.present("goal-row-action"), self.args.timeout)
        self.shot("failed", shots)
        outcomes["failed"] = {"statusReason": self.goal().get("statusReason")}
        self.remove_goal(shots)
        return outcomes

    def run_review(self, shots):
        def run():
            runs = self.fixture.get("/api/auto-review").get("runs", [])
            return next((item for item in runs if item.get("originalSessionID") == self.raw_id), None)

        def start_auto():
            self.type_and_send("/handoff-review")
            until("review dialog did not open", lambda: self.present("review-dialog-start"), self.args.timeout)
            self.click("review-dialog-auto")
            self.shot("review-dialog", shots)
            self.click("review-dialog-start")
            until("review loop did not start", lambda: run() is not None, self.args.timeout)
            until("review row did not appear", lambda: self.present("auto-review-strip"), self.args.timeout)

        start_auto()
        self.shot("review-running", shots)
        steps = []

        def clean():
            current = run()
            if current and current.get("status") == "running" and (not steps or steps[-1] != current.get("step")):
                steps.append(current.get("step"))
            return current is not None and current.get("status") == "clean"
        until("review loop did not end clean", clean, GOAL_TIMEOUT)
        until("review row did not offer Dismiss", lambda: self.present("auto-review-dismiss"), self.args.timeout)
        self.shot("review-clean", shots)
        finished = run()
        session = self.fixture.data(f"/api/session/{self.session_id}", dict)
        review_id = (session.get("metadata") or {}).get("openchamber", {}).get("reviewSessionID")
        if review_id != finished.get("reviewSessionID"):
            raise RuntimeError("the review session is not linked from the reviewed session")
        self.click("auto-review-dismiss")
        until("dismissed run stayed on the server", lambda: run() is None, self.args.timeout)
        until("review row stayed", lambda: not self.present("auto-review-strip"), self.args.timeout)

        start_auto()
        self.click("auto-review-stop")
        until("stopped run stayed on the server", lambda: run() is None, self.args.timeout)
        until("review row stayed after Stop", lambda: not self.present("auto-review-strip"), self.args.timeout)
        self.shot("review-stopped", shots)
        return {"clean": {"round": finished.get("round"), "stepsSeen": steps, "reviewSession": review_id},
                "stopped": True}

    def run(self):
        session = self.fixture.create_session(MODEL)
        self.raw_id = session["id"]
        self.session_id = urllib.parse.quote(session["id"], safe="")
        self.start("first")
        shots = []
        outcomes = {"goals": self.run_goals(shots)}
        until("the session did not settle", self.settled, GOAL_TIMEOUT)
        outcomes["review"] = self.run_review(shots)
        outcomes["screenshots"] = shots
        return outcomes


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    inputs = {"native": identity, "opencode": binary_info(args.opencode_binary),
              "dist": runner.artifact_identity(args.dist), "script": binary_info(Path(__file__).resolve()),
              "fixtureScript": binary_info(runner.ISOLATED_SERVER),
              "providerScript": binary_info(runner.ISOLATED_SERVER.with_name("fixture-provider.mjs"))}
    result = {"passed": False, "status": "failed", "display": "owned-xvfb", "scale": args.scale, "inputs": inputs}
    evidence = args.output / "goals-review"
    previous = accessibility_enabled()
    interaction = None
    try:
        with Interrupts() as interrupts:
            set_accessibility(True)
            root = Path("/tmp/opencode") / f"native-goals-review-fixture-{uuid.uuid4().hex}"
            result["fixtureRoot"] = str(root)
            with WorkflowFixture(root, evidence, args, interrupts) as fixture, ExitStack() as cleanup:
                interaction = Goals(fixture, args, identity, cleanup)
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
        except Exception as error:
            result.update(passed=False, status="failed", finalizationError=str(error))
        if interaction:
            result["nativeProcesses"] = [{"pid": client.process.pid, "exitCode": client.process.poll()}
                                         for client in interaction.clients if client.process is not None]
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
    args.output = (args.output or Path("/tmp/opencode") / f"native-goals-review-{uuid.uuid4().hex}").absolute()
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
