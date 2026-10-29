"""Qualify native permissions and question forms against an owned real engine.

Each check creates a fresh /tmp/opencode fixture. Only session creation uses
HTTP mutation; prompts, permissions and answers go through the native window.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time
import traceback
import urllib.parse
import urllib.request
import uuid

from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient, _atspi, accessibility_enabled, set_accessibility
from native_platform import binary_info, command, editable_text
import runner
import virtual_display

CHECKS = ("reject", "once", "always", "question", "dismiss", "supersede")
SHELL_COMMAND = "printf 'native-permission-ok\\n'"
SHELL_OUTPUT = "native-permission-ok\n"
CUSTOM_ANSWER = "Native fixture 你好 👩‍💻"
EXPECTED_ANSWER = {"q0": "Beta", "q1": ["One", "Two"], "q2": CUSTOM_ANSWER}


class RunInterrupted(Exception):
    pass


class Interrupts:
    """Defer delivery while registering a newly launched process for cleanup."""

    def __enter__(self):
        self.pending = None
        self.delivered = False
        self.depth = 0
        self.previous = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        for sig in self.previous:
            signal.signal(sig, self.receive)
        return self

    def receive(self, sig, _frame):
        self.pending = self.pending or sig
        self.check()

    def check(self):
        if self.pending and not self.depth and not self.delivered:
            self.delivered = True
            raise RunInterrupted(f"Interrupted by {signal.Signals(self.pending).name}")

    @contextmanager
    def defer(self):
        self.depth += 1
        try:
            yield
        finally:
            self.depth -= 1
            self.check()

    def __exit__(self, *_):
        for sig, previous in self.previous.items():
            signal.signal(sig, previous)


def until(label, read, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = read()
        if result:
            return result
        time.sleep(0.1)
    raise TimeoutError(label)


def process_identity(pid):
    try:
        # comm may contain spaces or parentheses; fields after its last ')' are stable.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return fields[19]  # starttime, field 22
    except (FileNotFoundError, ProcessLookupError):
        return None


def record_descendants(pid, owned):
    identity = process_identity(pid)
    if identity is None:
        return
    if pid in owned and owned[pid] != identity:
        return
    owned[pid] = identity
    try:
        children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
    except (FileNotFoundError, ProcessLookupError):
        return
    for child in children:
        record_descendants(int(child), owned)


def verify_readiness(value, root):
    if not isinstance(value, dict):
        raise ValueError("fixture readiness is not an object")
    for key, expected in (("root", root), ("project", root / "project"), ("state", root / "state")):
        if value.get(key) != str(expected) or Path(value[key]).resolve() != expected:
            raise ValueError(f"fixture {key} escaped the owned root")
    url = urllib.parse.urlsplit(value.get("url", ""))
    if url.scheme != "http" or url.hostname != "127.0.0.1" or not url.port or url.path or url.query or url.fragment or url.username is not None or url.password is not None:
        raise ValueError("fixture URL must be a loopback origin")
    for key in ("pid", "fixturePid", "fixturePort"):
        if type(value.get(key)) is not int or value[key] <= 0:
            raise ValueError(f"missing fixture {key}")
    return value


class Fixture:
    """Own the existing --interactions CLI, including failed startup."""

    MODE = "--interactions"

    def __init__(self, root, evidence, args, interrupts):
        self.root, self.evidence, self.args, self.interrupts = root, evidence, args, interrupts
        self.process = None
        self.owned = {}
        self.ready = None

    def __enter__(self):
        self.evidence.mkdir()
        self.log = (self.evidence / "fixture-cli.log").open("w")
        env = {key: os.environ[key] for key in ("PATH", "LANG") if key in os.environ}
        for key, folder in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                            ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")):
            target = self.evidence / "launcher" / folder
            target.mkdir(parents=True, mode=0o700)
            env[key] = str(target)
        try:
            with self.interrupts.defer():
                self.process = subprocess.Popen([
                    "node", str(runner.ISOLATED_SERVER), self.MODE, "--root", str(self.root),
                    "--dist", str(self.args.dist), "--opencode-binary", str(self.args.opencode_binary),
                ], cwd=self.evidence, env=env, stdout=subprocess.PIPE, stderr=self.log, start_new_session=True)
                record_descendants(self.process.pid, self.owned)
            with selectors.DefaultSelector() as selector:
                selector.register(self.process.stdout, selectors.EVENT_READ)
                buffer = b""
                end = time.monotonic() + 120
                while time.monotonic() < end:
                    record_descendants(self.process.pid, self.owned)
                    if self.process.poll() is not None:
                        raise RuntimeError(f"fixture exited during startup; see {self.log.name}")
                    if not selector.select(0.1):
                        continue
                    buffer += os.read(self.process.stdout.fileno(), 65536)
                    if b"\n" not in buffer:
                        continue
                    self.ready = verify_readiness(json.loads(buffer.split(b"\n", 1)[0]), self.root)
                    record_descendants(self.process.pid, self.owned)
                    for key in ("pid", "fixturePid"):
                        if self.ready[key] not in self.owned or process_identity(self.ready[key]) != self.owned[self.ready[key]]:
                            raise RuntimeError("fixture readiness named an unowned process")
                    break
                else:
                    raise TimeoutError("fixture readiness timed out")
            self.url = self.ready["url"]
            self.project = Path(self.ready["project"])
            self.state = Path(self.ready["state"])
            (self.evidence / "fixture.json").write_text(json.dumps(self.ready, indent=2))
            until("OpenCode did not become ready", lambda: self.get("/health").get("isOpenCodeReady"), 60)
            record_descendants(self.process.pid, self.owned)
            return self
        except BaseException:
            with self.interrupts.defer():
                self.close()
            raise

    def get(self, path):
        request = urllib.request.Request(self.url + path, headers={"x-opencode-directory": urllib.parse.quote(str(self.project), safe="")})
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def data(self, path, kind):
        result = self.get(path)
        if not isinstance(result, dict) or not isinstance(result.get("data"), kind):
            raise ValueError(f"malformed observation at {path}")
        return result["data"]

    def create_session(self, model):
        body = {"title": f"Native {self.root.name}", "agent": "build",
                "model": {"providerID": "perf", "id": model}, "location": {"directory": str(self.project)}}
        request = urllib.request.Request(self.url + "/api/session", data=json.dumps(body).encode(),
                                         headers={"Content-Type": "application/json"}, method="POST")
        with urllib.request.urlopen(request, timeout=10) as response:
            session = json.load(response)["data"]
        if session.get("location", {}).get("directory") != str(self.project):
            raise ValueError("created session is outside the owned private project")
        return session

    def close(self):
        if self.process:
            record_descendants(self.process.pid, self.owned)
            if self.process.poll() is None:
                self.process.send_signal(signal.SIGTERM)
                try:
                    self.process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    pass
            # The CLI normally closes everything. Limit fallback kills to the
            # exact recorded process identities, even if a parent failed early.
            survivors = [pid for pid, identity in self.owned.items() if process_identity(pid) == identity]
            for pid in reversed(survivors):
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            self.process.wait(timeout=10)
            self.process.stdout.close()
            if survivors:
                self.log.write(f"Forced cleanup of recorded processes: {survivors}\n")
            remaining = []
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                remaining = [pid for pid, identity in self.owned.items() if process_identity(pid) == identity]
                if not remaining:
                    break
                time.sleep(0.05)
            (self.evidence / "cleanup.json").write_text(json.dumps({"ownedPids": list(self.owned),
                "forcedPids": survivors, "remainingPids": remaining}, indent=2))
            if remaining:
                self.log.close()
                raise RuntimeError(f"owned fixture processes remain after cleanup: {remaining}")
        self.log.close()

    def __exit__(self, *_):
        with self.interrupts.defer():
            self.close()


def tool_states(messages, name):
    return [part["state"] for message in messages if message.get("type") == "assistant"
            for part in message.get("content", []) if part.get("type") == "tool" and part.get("name") == name]


def successful_shell_outputs(messages):
    """Only actual completed output with a zero exit proves command execution."""
    return [state for state in tool_states(messages, "shell")
            if state.get("status") == "completed" and state.get("input", {}).get("command") == SHELL_COMMAND
            and state.get("metadata", {}).get("exit") == 0
            and not state.get("metadata", {}).get("timeout") and not state.get("metadata", {}).get("signal")
            and "".join(part.get("text", "") for part in state.get("content", []) if part.get("type") == "text") == SHELL_OUTPUT]


def verify_permission(request, session_id):
    if request.get("sessionID") != session_id or request.get("action") != "shell" or request.get("resources") != [SHELL_COMMAND]:
        raise ValueError("permission is not the exact owned printf request")
    if not isinstance(request.get("id"), str) or not request["id"].startswith("per"):
        raise ValueError("permission has no valid ID")
    return request["id"]


def verify_form(form, session_id):
    if form.get("sessionID") != session_id or form.get("metadata", {}).get("kind") != "question":
        raise ValueError("form is not the owned fixture question")
    fields = form.get("fields", [])
    if [(field.get("key"), field.get("type"), field.get("custom")) for field in fields] != [
            ("q0", "string", True), ("q1", "multiselect", True), ("q2", "string", True)]:
        raise ValueError("unexpected question fields")
    if [[option.get("value") for option in field.get("options", [])] for field in fields] != [["Alpha", "Beta"], ["One", "Two"], []]:
        raise ValueError("unexpected question options")
    if not isinstance(form.get("id"), str) or not form["id"].startswith("frm_"):
        raise ValueError("form has no valid ID")
    return form["id"]


class Interaction:
    def __init__(self, fixture, args, identity, cleanup):
        self.fixture, self.args, self.identity, self.cleanup = fixture, args, identity, cleanup
        self.client = None
        self.clients = []
        self.clipboard = ClipboardProbe()
        self.prompts = 0

    def start(self, label):
        virtual_display.verify_owned_display()
        if binary_info(self.args.binary) != self.identity:
            raise RuntimeError("native binary changed during qualification")
        env = virtual_display.child_environment(runner.native_env(self.fixture.state, self.fixture.url, "openchamber-dark"))
        self.env = env
        log = self.cleanup.enter_context((self.fixture.evidence / f"native-{label}.log").open("w"))
        client = NativeClient(self.args.binary, env, self.args.scale, 1280, 800, stderr=log)
        def close_client():
            with self.fixture.interrupts.defer():
                if client.process is not None and client.process.poll() is None:
                    client.close()
        self.cleanup.callback(close_client)
        self.client = client
        self.clients.append(client)
        # NativeClient inherits cwd; keep the launch in this run's private project.
        previous = Path.cwd()
        try:
            os.chdir(self.fixture.project)
            with self.fixture.interrupts.defer():
                client.launch()
        finally:
            os.chdir(previous)
        client.wait_ready(45, connected=True)
        row = until("owned session did not appear", lambda: next((row for row in client.rows()
                    if row["key"].endswith(f":session:{self.session_id}")), None))
        client.click(row["key"])
        if not runner.wait_selected(client, row["key"]):
            raise RuntimeError("native client selected another session")
        client.wait_for_marker("composer-usable", 20)

    def click(self, target):
        if not self.client.wait_for({"elements": [target]}, self.args.timeout):
            raise TimeoutError(f"native control did not appear: {target}")
        self.client.click(target)

    def paste(self, node, text):
        self.client._activate()
        x, y = self.client._screen_point(node)
        command(self.env, "xdotool", "mousemove", str(x), str(y), "click", "1")
        until("text field did not focus", lambda: node.get_state_set().contains(_atspi().StateType.FOCUSED))
        self.clipboard.paste(text, lambda: command(self.env, "xdotool", "key", "ctrl+v"), lambda: editable_text(node) == text)

    def messages(self):
        return self.fixture.data(f"/api/session/{self.session_id}/message?limit=100", list)

    def pending(self, kind):
        return self.fixture.data(f"/api/session/{self.session_id}/{kind}", list)

    def composer(self):
        entries = []
        stack = [(self.client._find("composer"), 0)]
        while stack:
            node, depth = stack.pop()
            if node is None or depth > 24:
                continue
            if node.get_role_name() == "entry":
                entries.append(node)
            for index in range(node.get_child_count()):
                stack.append((node.get_child_at_index(index), depth + 1))
        if len(entries) != 1:
            raise RuntimeError("expected exactly one entry inside the composer")
        return entries[0]

    def send(self, *, superseding=False):
        if not superseding and (self.pending("permission") or self.pending("form")):
            raise RuntimeError("cannot send another prompt while an interaction is pending")
        text = f"Native interaction fixture turn {self.prompts + 1}."
        self.paste(self.composer(), text)
        until("send control did not enable", lambda: self.client._find("composer-submit").get_state_set().contains(_atspi().StateType.ENABLED))
        self.client.key("Return")
        self.prompts += 1
        until("prompt was not admitted exactly once", lambda: len([message for message in self.messages()
              if message.get("type") == "user" and message.get("text") == text]) == 1, self.args.timeout)

    def settled(self):
        return self.session_id not in self.fixture.data("/api/session/active", dict)

    def shell_done(self, count, no_ask=False):
        def observed():
            if no_ask and self.pending("permission"):
                raise RuntimeError("saved approval prompted again")
            return len(successful_shell_outputs(self.messages())) == count and self.settled()
        until("shell did not complete with the exact marker and zero exit", observed, self.args.timeout)

    def permission(self):
        requests = until("real permission request did not arrive", lambda: self.pending("permission"), self.args.timeout)
        if len(requests) != 1:
            raise RuntimeError("expected exactly one owned permission")
        session = self.fixture.data(f"/api/session/{self.session_id}", dict)
        if session.get("location", {}).get("directory") != str(self.fixture.project):
            raise ValueError("session left its private project")
        request_id = verify_permission(requests[0], self.session_id)
        (self.fixture.evidence / f"permission-{request_id}.json").write_text(json.dumps(requests[0], indent=2))
        return request_id

    def rejected(self, successes):
        def observed():
            states = tool_states(self.messages(), "shell")
            return any(state.get("status") == "error" for state in states) and not self.pending("permission") and self.settled()
        until("rejection did not settle the command", observed, self.args.timeout)
        if len(successful_shell_outputs(self.messages())) != successes:
            raise RuntimeError("rejected command produced successful output")

    def run(self, check):
        session = self.fixture.create_session("interaction-question" if check in ("question", "dismiss", "supersede") else "interaction-shell")
        self.session_id = urllib.parse.quote(session["id"], safe="")
        self.start("first")
        self.send()
        if check in ("reject", "once", "always"):
            request_id = self.permission()
            self.click(f"permission-{request_id}-{check}")
            if check == "reject":
                self.rejected(0)
            else:
                self.shell_done(1)
                self.send()
                if check == "once":
                    repeated = self.permission()
                    if repeated == request_id:
                        raise RuntimeError("once reused the settled permission ID")
                    self.click(f"permission-{repeated}-reject")
                    self.rejected(1)
                else:
                    saved = self.fixture.data("/api/permission/saved?" + urllib.parse.urlencode({"projectID": session["projectID"]}), list)
                    if not any(rule.get("action") == "shell" for rule in saved):
                        raise RuntimeError("always did not save a shell approval")
                    self.shell_done(2, no_ask=True)
                    self.click("window-close")
                    self.client.process.wait(timeout=15)
                    if self.client.process.returncode != 0:
                        raise RuntimeError("native close failed")
                    self.start("reopened")
                    self.send()
                    self.shell_done(3, no_ask=True)
        else:
            forms = until("real question form did not arrive", lambda: self.pending("form"), self.args.timeout)
            if len(forms) != 1:
                raise RuntimeError("expected exactly one owned question form")
            form_id = verify_form(forms[0], session["id"])
            (self.fixture.evidence / f"form-{form_id}.json").write_text(json.dumps(forms[0], indent=2))
            if check == "supersede":
                old_form_id = form_id
                self.send(superseding=True)
                old_path = f"/api/session/{self.session_id}/form/{old_form_id}"
                until("superseding send did not cancel the old form", lambda:
                      self.fixture.data(old_path, dict).get("state", {}).get("status") == "cancelled", self.args.timeout)

                def replacement():
                    found = [item for item in self.pending("form") if item.get("id") != old_form_id]
                    if len(found) > 1:
                        raise RuntimeError("superseding send created multiple question forms")
                    return found[0] if found else None

                form = until("replacement question did not arrive", replacement, self.args.timeout)
                form_id = verify_form(form, session["id"])
                (self.fixture.evidence / f"form-{form_id}.json").write_text(json.dumps(form, indent=2))
                self.click(f"form-{form_id}-cancel")
            elif check == "dismiss":
                self.click(f"form-{form_id}-cancel")
            else:
                self.click(f"form-option-{form_id}-q0-1")
                self.click(f"form-{form_id}-next")
                self.click(f"form-option-{form_id}-q1-0")
                self.click(f"form-option-{form_id}-q1-1")
                self.click(f"form-{form_id}-next")
                self.click(f"form-{form_id}-back")
                self.click(f"form-{form_id}-next")
                target = f"form-input-{form_id}-q2"
                if not self.client.wait_for({"elements": [target]}, self.args.timeout):
                    raise TimeoutError("custom answer input did not appear")
                self.paste(self.client._find(target), CUSTOM_ANSWER)
                self.click(f"form-{form_id}-submit")
            detail_path = f"/api/session/{self.session_id}/form/{form_id}"
            status = "cancelled" if check in ("dismiss", "supersede") else "answered"
            until("form did not reach the expected terminal state", lambda: self.fixture.data(detail_path, dict).get("state", {}).get("status") == status, self.args.timeout)
            if check == "question":
                if self.fixture.data(detail_path, dict)["state"].get("answer") != EXPECTED_ANSWER:
                    raise RuntimeError("form answers changed or lost Unicode/multiple selections")
                until("question tool did not preserve answers", lambda: any(state.get("status") == "completed"
                      and state.get("metadata", {}).get("answers") == [["Beta"], ["One", "Two"], [CUSTOM_ANSWER]]
                      for state in tool_states(self.messages(), "question")), self.args.timeout)
            until("question did not settle", lambda: not self.pending("form") and self.settled(), self.args.timeout)
        users = [message for message in self.messages() if message.get("type") == "user"]
        if len(users) != self.prompts:
            raise RuntimeError("native interaction duplicated or lost a prompt")
        (self.fixture.evidence / "messages.json").write_text(json.dumps(self.messages(), indent=2, ensure_ascii=False))
        self.client.screenshot(self.fixture.evidence / "completed.png")
        return {"sessionID": session["id"], "prompts": self.prompts,
                "inputSubstitutions": [item for client in self.clients for item in client.substitutions]}


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    print(f"Report: {args.output / 'report.json'}", flush=True)
    identity = binary_info(args.binary)
    engine = binary_info(args.opencode_binary)
    results = []
    previous_a11y = accessibility_enabled()
    try:
        with Interrupts() as interrupts:
            set_accessibility(True)
            for check in args.checks:
                root = Path("/tmp/opencode") / f"native-interaction-{check}-{uuid.uuid4().hex}"
                evidence = args.output / check
                interaction = None
                result = {"check": check, "fixtureRoot": str(root), "passed": False, "status": "failed"}
                try:
                    with Fixture(root, evidence, args, interrupts) as fixture, ExitStack() as cleanup:
                        interaction = Interaction(fixture, args, identity, cleanup)
                        try:
                            result["detail"] = interaction.run(check)
                        except BaseException:
                            if interaction.client and interaction.client.process and interaction.client.process.poll() is None:
                                try:
                                    interaction.client.screenshot(evidence / "failed.png")
                                except Exception as error:
                                    result["screenshotError"] = str(error)
                            raise
                    result.update(passed=True, status="passed")
                except (Exception, KeyboardInterrupt) as error:
                    result.update(passed=False, status="failed")
                    result["error"] = str(error)
                    (evidence / "failure.txt").write_text(traceback.format_exc())
                    if isinstance(error, (RunInterrupted, KeyboardInterrupt)):
                        result["interrupted"] = True
                if interaction:
                    result["nativeProcesses"] = [{"pid": client.process.pid, "exitCode": client.process.poll()}
                                                 for client in interaction.clients if client.process is not None]
                    result["inputSubstitutions"] = [item for client in interaction.clients for item in client.substitutions]
                    if result["passed"] and result["inputSubstitutions"]:
                        result.update(passed=False, status="incomplete", error="pointer input required an accessibility substitution")
                results.append(result)
                if result.get("interrupted"):
                    break
    finally:
        restoration_error = None
        try:
            set_accessibility(previous_a11y)
        except (OSError, subprocess.SubprocessError) as error:
            # The display wrapper may already have stopped its private bus on
            # interruption. Keep the report even when restoration is impossible.
            restoration_error = str(error)
            for result in results:
                if result["passed"]:
                    result.update(passed=False, status="failed", error="private accessibility state could not be restored")
        try:
            unchanged = binary_info(args.binary) == identity and binary_info(args.opencode_binary) == engine
        except OSError:
            unchanged = False
        if not unchanged:
            results.append({"check": "binary-identity", "passed": False, "error": "binary changed during qualification"})
        (args.output / "report.json").write_text(json.dumps({"binary": identity, "opencodeBinary": engine,
            "scale": args.scale, "display": "owned-xvfb", "accessibilityRestoreError": restoration_error,
            "checks": results}, indent=2, ensure_ascii=False))
        for result in results:
            print(f"{result['check']}: {result.get('status', 'passed' if result['passed'] else 'failed')}", flush=True)
    return 0 if not restoration_error and results and all(item["passed"] for item in results) and len(results) == len(args.checks) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runner.DEFAULT_NATIVE)
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1)
    parser.add_argument("--checks", default=",".join(CHECKS))
    parser.add_argument("--timeout", type=float, default=20)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    args.checks = args.checks.split(",")
    if not args.checks or any(check not in CHECKS for check in args.checks) or len(set(args.checks)) != len(args.checks):
        parser.error("--checks must contain unique names from " + ",".join(CHECKS))
    for name in ("binary", "opencode_binary", "dist"):
        setattr(args, name, getattr(args, name).resolve())
    for path in (args.binary, args.opencode_binary):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"executable is missing: {path}")
    args.output = (args.output or Path("/tmp/opencode") / f"native-interactions-{uuid.uuid4().hex}").absolute()
    if args.output.parent != Path("/tmp/opencode") or args.output.parent.resolve() != Path("/tmp/opencode") or args.output.exists():
        parser.error("--output must be a new directory directly under /tmp/opencode")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if not virtual_display.inside():
        if problem := virtual_display.available():
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    sys.exit(main())
