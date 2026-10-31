#!/usr/bin/env python3
"""Qualify native password sign-in against a private real OpenChamber server."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.request
import uuid

from native_adapter import NativeClient, _atspi
from native_interactions import Fixture, Interaction, Interrupts, set_accessibility, accessibility_enabled, until
from native_platform import binary_info, command
from native_platform import editable_text
import runner
import virtual_display
from secret_service_fixture import SecretServiceFixture


# Public synthetic fixture data, not a credential for a user's server.
PASSWORD = "native-fixture-auth-only"


class AuthenticationFixture(Fixture):
    MODE = "--authentication"


def verify_password_requirement(fixture):
    try:
        fixture.get("/auth/session")
    except urllib.error.HTTPError as error:
        with error:
            payload = json.load(error)
        if error.code == 401 and payload.get("authenticated") is False and payload.get("locked") is True:
            return
    raise RuntimeError("fixture did not require authentication")


def revoke_native_client(fixture):
    """An independent cookie session revokes only this fixture's desktop token."""
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor())
    request = urllib.request.Request(fixture.url + "/auth/session", method="POST",
        data=json.dumps({"password": PASSWORD}).encode(), headers={"Content-Type": "application/json"})
    with opener.open(request, timeout=10) as response:
        if not json.load(response).get("authenticated"):
            raise RuntimeError("fixture operator could not authenticate")
    with opener.open(fixture.url + "/api/client-auth/clients", timeout=10) as response:
        clients = [item for item in json.load(response)["clients"]
                   if item.get("clientKind") == "desktop" and not item.get("revokedAt")]
    if len(clients) != 1:
        raise RuntimeError("expected exactly one fixture desktop credential")
    request = urllib.request.Request(fixture.url + "/api/client-auth/clients/" + clients[0]["id"], method="DELETE")
    with opener.open(request, timeout=10) as response:
        if not json.load(response).get("revoked"):
            raise RuntimeError("fixture token was not revoked")


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    result = {"passed": False, "display": "owned-xvfb", "binary": identity}
    previous = accessibility_enabled()
    client = None
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            cleanup.enter_context(SecretServiceFixture(args.output / "secret-service"))
            root = Path("/tmp/opencode") / f"native-auth-{uuid.uuid4().hex}"
            fixture = cleanup.enter_context(AuthenticationFixture(root, args.output / "fixture", args, interrupts))
            verify_password_requirement(fixture)
            env = virtual_display.child_environment(runner.native_env(args.output / "native-state", fixture.url, "openchamber-dark"))
            # An inherited developer password must not bypass the login UI.
            env.pop("OPENCHAMBER_NATIVE_PASSWORD", None)
            clients = []
            def launch(label):
                log = cleanup.enter_context((args.output / f"native-{label}.log").open("w"))
                launched = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                clients.append(launched)
                cleanup.callback(launched.close)
                previous_directory = Path.cwd()
                try:
                    os.chdir(fixture.project)
                    launched.launch()
                finally:
                    os.chdir(previous_directory)
                launched.wait_ready(40, connected=False)
                return launched
            client = launch("first")
            client.screenshot(args.output / "initial.png")
            if not client.wait_for({"elements": ["authentication-password", "authentication-unlock"]}, 15):
                raise RuntimeError("the password sign-in controls are missing")
            client.screenshot(args.output / "password-required.png")
            def enter_password(text):
                client.click("authentication-password")
                command(env, "xdotool", "key", "ctrl+a")
                command(env, "xdotool", "type", "--clearmodifiers", "--delay", "15", text)
                if text in (client._find("authentication-password").get_name() or ""):
                    raise RuntimeError("password appeared in its accessibility label")
                text_interface = client._find("authentication-password").get_text_iface()
                exposed = _atspi().Text.get_text(text_interface, 0, -1) if text_interface is not None else ""
                if text in exposed:
                    raise RuntimeError("password appeared in its accessibility value")
                until("unlock did not become enabled", lambda: client._find("authentication-unlock")
                      .get_state_set().contains(_atspi().StateType.ENABLED), 10)
                client.click("authentication-unlock")

            enter_password("deliberately-wrong-fixture-password")
            until("incorrect password was not reported", lambda: "Incorrect password" in (
                client._find("authentication-error").get_name() or ""), 15)
            client.screenshot(args.output / "password-rejected.png")
            enter_password(PASSWORD)
            client.wait_for_marker("sessions-shown", 40)
            until("authenticated workspace did not appear", lambda: "authentication-password" not in {
                node.get_accessible_id() for node in client._walk()
            }, 15)
            client.screenshot(args.output / "authenticated.png")
            client.close()
            client = launch("session-only-reopened")
            if not client.wait_for({"elements": ["authentication-password"]}, 15):
                raise RuntimeError("session-only sign-in unexpectedly survived restart")
            client.click("authentication-trust")
            enter_password(PASSWORD)
            client.wait_for_marker("sessions-shown", 40)
            client.close()
            client = launch("trusted-reopened")
            client.wait_for_marker("sessions-shown", 40)
            if "authentication-password" in {node.get_accessible_id() for node in client._walk()}:
                raise RuntimeError("trusted sign-in was not restored")
            client.screenshot(args.output / "trusted-reopened.png")
            probe = object.__new__(Interaction)
            probe.client = client
            composer = until("composer did not appear after sign-in", probe.composer, 15)
            until("composer did not become editable", lambda: composer.get_state_set().contains(_atspi().StateType.EDITABLE), 15)
            client._activate()
            x, y = client._screen_point(composer)
            command(env, "xdotool", "mousemove", str(x), str(y), "click", "1")
            until("composer did not focus", lambda: composer.get_state_set().contains(_atspi().StateType.FOCUSED), 10)
            draft = "Keep this draft after token revocation."
            command(env, "xdotool", "type", "--clearmodifiers", draft)
            until("draft was not entered before revocation", lambda: editable_text(probe.composer()) == draft, 10)
            client.screenshot(args.output / "draft-before-revocation.png")
            revoke_native_client(fixture)
            client.click("rail-files")
            if not client.wait_for({"elements": ["authentication-password"]}, 15):
                raise RuntimeError("an HTTP request after revocation did not return to sign-in")
            client.screenshot(args.output / "revoked.png")
            if client._find("authentication-trust").get_state_set().contains(_atspi().StateType.CHECKED):
                client.click("authentication-trust")
            enter_password(PASSWORD)
            until("reauthentication did not restore the workspace", probe.composer, 30)
            client.screenshot(args.output / "reauthenticated.png")
            if editable_text(probe.composer()) != draft:
                raise RuntimeError("authentication recovery lost the unsent draft")
            result["checks"] = ["password-required", "wrong-password", "secret-accessibility", "session-only-restart",
                                "trusted-restart", "http-revocation", "draft-preserved"]
            for path in (args.output / "native-state").rglob("*"):
                if path.is_file() and PASSWORD.encode() in path.read_bytes():
                    raise RuntimeError("password was persisted in native application state")
            result["inputSubstitutions"] = [item for current in clients for item in current.substitutions]
            if result["inputSubstitutions"]:
                raise RuntimeError("input required an accessibility substitution")
        result["passed"] = True
    except Exception as error:
        result["error"] = str(error)
    finally:
        try:
            set_accessibility(previous)
        except Exception:
            result.update(passed=False, error="private accessibility state could not be restored")
        if binary_info(args.binary) != identity:
            result.update(passed=False, error="binary changed during qualification")
        (args.output / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for name in ("binary", "dist", "opencode_binary", "output"):
        setattr(args, name, getattr(args, name).resolve())
    if args.output.parent != Path("/tmp/opencode") or args.output.exists():
        parser.error("--output must be a new directory directly under /tmp/opencode")
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    raise SystemExit(main())
