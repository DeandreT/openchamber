"""Real-window model, agent and effort qualification on a fresh local fixture.

Run: python3 scripts/parity/native_selections.py --scale 1.25
The installed candidate must implement the pickers. A legacy binary fails.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import signal
import sys
import traceback
import urllib.parse
import uuid

from native_adapter import _atspi, accessibility_enabled, set_accessibility
from native_interactions import Fixture, Interaction, Interrupts, RunInterrupted, until
from native_platform import binary_info, command, editable_text
import runner
import virtual_display

MODEL = "stream-1200cps"
AGENT = "fixture-beta"
PROMPT = "Selection fixture 你好 👩‍💻 café"


class SelectionFixture(Fixture):
    """Use the same owned server and cleanup, with its opt-in selection config."""

    MODE = "--selections"


def verify_turn(messages, prompt, model, agent, variant):
    """Verify chronological history against the engine's canonical model ref."""
    users = [item for item in messages if item.get("type") == "user" and item.get("text") == prompt]
    if len(users) != 1:
        raise ValueError("prompt was not admitted exactly once")
    index = messages.index(users[0])
    following = messages[index + 1:]
    next_user = next((i for i, item in enumerate(following) if item.get("type") == "user"), len(following))
    steps = [item for item in following[:next_user] if item.get("type") == "assistant"]
    expected = {"providerID": "perf", "id": model, "variant": "default" if variant is None else variant}
    if not steps:
        raise ValueError("no assistant step follows the prompt")
    for step in steps:
        if step.get("agent") != agent or step.get("model") != expected:
            raise ValueError("assistant used a different model, agent or effort")
        if step.get("error") or not step.get("time", {}).get("completed") or step.get("finish") != "stop":
            raise ValueError("assistant step did not complete successfully")
        if any(part.get("type") == "tool" for part in step.get("content", [])):
            raise ValueError("selection fixture unexpectedly called a tool")
        if not any(part.get("type") == "text" and part.get("text") for part in step.get("content", [])):
            raise ValueError("assistant produced no text")
    return {"userID": users[0]["id"], "assistantIDs": [step["id"] for step in steps],
            "model": expected, "agent": agent}


def require_clean_result(clients):
    if any(client.substitutions for client in clients):
        raise RuntimeError("input used an accessibility action substitution")
    if any(client.process is not None and client.process.poll() is None for client in clients):
        raise RuntimeError("native process remained after cleanup")
    if any(client.process is not None and client.process.poll() not in (0, -signal.SIGTERM) for client in clients):
        raise RuntimeError("native process crashed or required forced cleanup")


class Selections(Interaction):
    def messages(self):
        # Pinned 2.2 returns newest-first; turn boundaries need chronological order.
        return list(reversed(super().messages()))

    def named(self, name, role):
        def find():
            matches = [node for node in self.client._walk()
                       if node.get_name() == name and node.get_role_name() == role]
            if len(matches) > 1:
                raise RuntimeError(f"ambiguous {role}: {name}")
            return matches[0] if matches else None
        return until(f"missing {role}: {name}", find, self.args.timeout)

    def pointer(self, node):
        self.client._activate()
        x, y = self.client._screen_point(node)
        command(self.env, "xdotool", "mousemove", str(x), str(y), "click", "1")

    def choose(self, kind, label, search=None):
        self.click(f"composer-{kind}")
        if search:
            self.paste(self.named(search, "entry"), label)
        self.pointer(self.named(label, "list item"))
        until(f"{kind} choice did not reach toolbar", lambda:
              self.client._find(f"composer-{kind}").get_name() == label, self.args.timeout)

    def labels(self):
        return {kind: self.client._find(f"composer-{kind}").get_name()
                for kind in ("model", "agent", "variant")}

    def reopen(self, label):
        self.click("window-close")
        self.client.process.wait(timeout=15)
        if self.client.process.returncode != 0:
            raise RuntimeError("native window close failed")
        self.start(label)

    def send_selection(self, text, variant):
        self.paste(self.composer(), text)
        until("send control did not enable", lambda:
              self.client._find("composer-submit").get_state_set().contains(_atspi().StateType.ENABLED))
        self.client.key("Return")
        until("exact Unicode prompt did not arrive", lambda: any(
            item.get("type") == "user" and item.get("text") == text for item in self.messages()), self.args.timeout)
        def completed():
            messages = self.messages()
            index = next(i for i, item in enumerate(messages) if item.get("type") == "user" and item.get("text") == text)
            return self.settled() and any(item.get("type") == "assistant" and item.get("time", {}).get("completed")
                                          for item in messages[index + 1:])
        until("selection turn did not finish", completed, self.args.timeout)
        self.prompts += 1
        return verify_turn(self.messages(), text, MODEL, AGENT, variant)

    def run(self):
        session = self.fixture.create_session("stream-20000cps")
        self.session_id = urllib.parse.quote(session["id"], safe="")
        self.start("first")
        self.choose("agent", "Fixture-alpha", "Search agents...")
        self.choose("agent", "Fixture-beta", "Search agents...")
        # Override beta's stream-20000cps default after choosing the agent.
        # A label-only model picker must fail the completed-step verification.
        self.choose("model", MODEL, "Search models...")
        self.choose("variant", "Default")
        self.choose("variant", "high")
        expected = self.labels()
        if expected != {"model": MODEL, "agent": "Fixture-beta", "variant": "high"}:
            raise RuntimeError("toolbar did not retain the selected model, agent and high effort")
        self.paste(self.composer(), PROMPT)
        self.reopen("unsent")
        if self.labels() != expected or editable_text(self.composer()) != PROMPT:
            raise RuntimeError("unsent selection or Unicode draft did not survive reopen")
        if any(item.get("type") == "user" for item in self.messages()):
            raise RuntimeError("reopening submitted an unsent draft")
        self.client.screenshot(self.fixture.evidence / "unsent.png")
        # Test cancellation in each picker and focus without clicking the composer.
        for kind, search, alternate in (("model", "Search models...", "stream-20000cps"),
                                        ("agent", "Search agents...", "Fixture-alpha"),
                                        ("variant", None, "Default")):
            self.click(f"composer-{kind}")
            if search:
                self.paste(self.named(search, "entry"), alternate)
            self.named(alternate, "list item")
            self.client.key("Escape")
            until("cancel did not restore composer focus", lambda:
                  self.composer().get_state_set().contains(_atspi().StateType.FOCUSED), self.args.timeout)
            if self.labels() != expected or editable_text(self.composer()) != PROMPT:
                raise RuntimeError("cancel changed the selection or draft")
        # Clear through native keys before pasting the send probe.
        self.pointer(self.composer())
        command(self.env, "xdotool", "key", "ctrl+a", "BackSpace")
        high = self.send_selection(PROMPT, "high")
        self.choose("variant", "Default")
        default_labels = self.labels()
        self.reopen("default")
        if self.labels() != default_labels or default_labels["variant"] != "Default":
            raise RuntimeError("explicit Default did not survive reopen")
        if len([item for item in self.messages() if item.get("type") == "user"]) != 1:
            raise RuntimeError("reopen duplicated a send")
        default = self.send_selection(PROMPT + " Default", None)
        messages = self.messages()
        if len([item for item in messages if item.get("type") == "user"]) != 2:
            raise RuntimeError("selection run duplicated or lost a prompt")
        (self.fixture.evidence / "messages.json").write_text(json.dumps(messages, indent=2, ensure_ascii=False))
        self.client.screenshot(self.fixture.evidence / "completed.png")
        return {"sessionID": session["id"], "high": high, "default": default,
                "unsentReopen": True, "cancelFocus": True, "defaultReopen": True}


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    inputs = {"native": identity, "opencode": binary_info(args.opencode_binary),
              "dist": runner.artifact_identity(args.dist), "script": binary_info(Path(__file__).resolve()),
              "fixtureScript": binary_info(runner.ISOLATED_SERVER),
              "providerScript": binary_info(runner.ISOLATED_SERVER.with_name("fixture-provider.mjs"))}
    for name in ("native_interactions.py", "native_adapter.py", "clipboard_probe.py", "virtual_display.py"):
        inputs[name] = binary_info(Path(__file__).with_name(name).resolve())
    result = {"passed": False, "status": "failed", "display": "owned-xvfb", "scale": args.scale, "inputs": inputs}
    evidence = args.output / "selections"
    previous = accessibility_enabled()
    interaction = None
    try:
        with Interrupts() as interrupts:
            set_accessibility(True)
            root = Path("/tmp/opencode") / f"native-selections-fixture-{uuid.uuid4().hex}"
            result["fixtureRoot"] = str(root)
            with SelectionFixture(root, evidence, args, interrupts) as fixture, ExitStack() as cleanup:
                interaction = Selections(fixture, args, identity, cleanup)
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
    args.output = (args.output or Path("/tmp/opencode") / f"native-selections-{uuid.uuid4().hex}").absolute()
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
