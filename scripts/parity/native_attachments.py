#!/usr/bin/env python3
"""Qualify a bounded native Attach-menu and clipboard-file workflow.

Run with --binary <frozen-native> --output /tmp/opencode/<new-name>.
The real menu is activated on the owned display. Linux's portal picker is
reported unsupported here: the private bus activates no desktop services.
A private text/uri-list producer supplies small text and PNG files instead.
This unit checks staging failure/retry, removal, saved captures across a normal
restart and exact uploaded bytes and prompt references. It grants no Electron
pixel, picker-selection, drop, IME, document-conversion or whole-close parity.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import traceback
import urllib.parse
import uuid

from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient, _atspi, accessibility_enabled, set_accessibility
from native_hosts import HostFixture
from native_interactions import Interaction, Interrupts, RunInterrupted, until
from native_platform import binary_info, command, editable_text
import png
import runner
import virtual_display
from workflow_proxy import FaultProxy

DRAFT = "Synthetic attachment draft. Keep its exact text."
TEXT_NAME = "native-attachment.txt"
PNG_NAME = "native-attachment.png"
TEXT_BYTES = "Synthetic attachment bytes.\r\n你好\n".encode()
WRITE_ROUTE = "/api/experimental/fs/write"
REQUIRED_PROOFS = ("menu", "clipboard-files", "retry", "remove", "restart-capture", "send-bytes")
PROFILE_KEYS = ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME")
BUTTON_ROLES = ("push button", "button")


class Unobserved(RuntimeError):
    """Required platform metadata or an input producer is unavailable."""


def save(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def validate_output(path):
    if path.parent != Path("/tmp/opencode") or path.parent.resolve() != path.parent or path.exists() or path.is_symlink():
        raise ValueError("--output must be a new directory directly under /tmp/opencode")


def byte_check(actual, expected):
    """Keep both digests even when exact byte equality fails."""
    return {"matches": actual == expected, "actualBytes": len(actual), "expectedBytes": len(expected),
            "actualSha256": hashlib.sha256(actual).hexdigest(), "expectedSha256": hashlib.sha256(expected).hexdigest()}


def final_status(proofs, cleanup_errors):
    if cleanup_errors or any(proof.get("status") == "failed" for proof in proofs.values()):
        return "failed"
    return "passed" if all(proofs.get(key, {}).get("status") == "passed" for key in REQUIRED_PROOFS) else "incomplete"


def text_node(node):
    """Text collection never requests the Component interface or geometry."""
    node.clear_cache()
    return {"id": node.get_accessible_id() or "", "name": node.get_name() or "", "role": node.get_role_name(),
            "states": [state.value_nick for state in node.get_state_set().get_states()]}


def guarded_geometry(client, node):
    """A non-Component node remains useful text evidence."""
    try:
        if node.get_component_iface() is None:
            return {"status": "unsupported", "reason": "AT-SPI Component is absent"}
        rect = client._rect(node)
        return {"status": "observed", "rect": rect}
    except Exception as error:
        return {"status": "unobserved", "error": str(error)}


def named_control(nodes, name, roles):
    matches = [node for node in nodes if node.get_name() == name and node.get_role_name() in roles]
    if len(matches) > 1:
        raise Unobserved(f"ambiguous AT-SPI control metadata for {name!r}")
    return matches[0] if matches else None


def file_row(nodes, name):
    matches = [node for node in nodes if re.fullmatch(r"attachment-(?:input-)?[A-Za-z0-9_]+", node.get_accessible_id() or "")
               and (node.get_name() or "").startswith(name + ". ")]
    if len(matches) > 1:
        raise ValueError(f"duplicate rendered attachment filename: {name}")
    return matches[0] if matches else None


def children(node):
    stack = [(node, 0)]
    while stack:
        current, depth = stack.pop()
        if current is None or depth > 24:
            continue
        yield current
        stack.extend((current.get_child_at_index(index), depth + 1) for index in reversed(range(current.get_child_count())))


def uri_list(paths, owned_inputs):
    """Only the driver's regular synthetic inputs can enter its file clipboard."""
    result = []
    for path in paths:
        if path.parent != owned_inputs or path.resolve() != path or path.is_symlink() or not path.is_file():
            raise ValueError("clipboard input escaped the owned synthetic directory")
        result.append(path.as_uri())
    if not result or len(set(result)) != len(result):
        raise ValueError("clipboard file list is empty or duplicated")
    return ("\r\n".join(result) + "\r\n").encode()


def paste_files(probe, paths, owned_inputs, dispatch, observed):
    """Serve URI bytes through GDK while the native client requests X11 selection data."""
    virtual_display.verify_owned_display()
    from gi.repository import Gdk

    if observed():
        raise ValueError("attachment paste result was already present")
    provider = Gdk.ContentProvider.new_for_bytes("text/uri-list", probe.glib.Bytes.new(uri_list(paths, owned_inputs)))
    if not probe.clipboard.set_content(provider):
        raise Unobserved("private clipboard producer could not offer text/uri-list")
    probe.display.sync()
    loop = probe.glib.MainLoop()
    outcome = {"finished": False, "expired": False, "error": None}

    def poll():
        try:
            outcome["finished"] = bool(observed())
        except Exception as error:
            outcome["error"] = error
        if outcome["finished"] or outcome["error"]:
            loop.quit()
            return False
        return True

    def expire():
        outcome["expired"] = True
        loop.quit()
        return False

    poll_id = probe.glib.timeout_add(20, poll)
    timer_id = probe.glib.timeout_add(20000, expire)
    try:
        dispatch()
        loop.run()
    finally:
        if not outcome["finished"] and outcome["error"] is None:
            probe.glib.source_remove(poll_id)
        if not outcome["expired"]:
            probe.glib.source_remove(timer_id)
    if outcome["error"]:
        raise outcome["error"]
    if not outcome["finished"]:
        raise TimeoutError("file-list paste did not render the expected attachments within 20 s")


def draft_capture(document, session, directory, expected):
    """Observe attachment ownership shared by journal versions 7 and 8."""
    if document.get("version") not in (7, 8):
        raise Unobserved("attachment capture requires the v7 or v8 journal schema")
    drafts = [draft for draft in document.get("drafts", [])
              if draft.get("owner", {}).get("target") == {"session": session}
              and draft.get("owner", {}).get("directory") == str(directory)]
    if len(drafts) != 1 or drafts[0].get("text") != DRAFT:
        raise ValueError("journal did not retain exactly one owned draft with its exact text")
    draft = drafts[0]
    content = draft.get("content", {}).get("owned")
    if not isinstance(content, dict) or not isinstance(content.get("files"), list):
        raise Unobserved("journal has no owned semantic attachment metadata")
    files = content["files"]
    if sorted(file.get("name", "") for file in files) != sorted(expected) or len({file.get("id") for file in files}) != len(files):
        raise ValueError("journal attachment names are missing, duplicated or unexpected")
    catalog = document.get("attachments")
    if not isinstance(catalog, list):
        raise Unobserved("journal has no prepared attachment catalog")
    result = {}
    for file in files:
        name = file["name"]
        if any(key not in file for key in ("id", "media_type", "byte_len", "source_document_id", "content")):
            raise Unobserved("journal attachment metadata is missing required fields")
        matches = [item for item in catalog if item.get("id") == file["id"]]
        if len(matches) != 1 or file["content"] != "prepared":
            raise ValueError("draft file has no unique prepared-byte owner")
        item = matches[0]
        blob, target = item.get("blob", {}), item.get("staging")
        if not isinstance(target, dict) or any(key not in target for key in
                ("runtime", "directory", "temporary_directory", "server_path", "uri", "filename", "phase")):
            raise Unobserved("prepared attachment has no complete captured staging target")
        data = expected[name]
        if (not re.fullmatch(r"[0-9a-f]{32}", file["id"]) or blob.get("id") != file["id"]
                or file["byte_len"] != len(data) or blob.get("byte_len") != len(data)
                or blob.get("sha256") != list(hashlib.sha256(data).digest())
                or file["media_type"] != ("image/png" if name.endswith(".png") else "text/plain") or file["source_document_id"] is not None
                or target["directory"] != str(directory) or target["runtime"] != draft["owner"].get("runtime")
                or target["filename"] != name or target["uri"] != Path(target["server_path"]).as_uri()
                or target["phase"] not in ("planned", "writing", "ready", "uncertain")):
            raise ValueError("captured attachment bytes, metadata or target ownership changed")
        result[name] = {"id": file["id"], "mediaType": file["media_type"], "byteLen": file["byte_len"],
                        "blobSha256": hashlib.sha256(data).hexdigest(), "staging": target}
    return result


def owned_server_path(target, temporary_directory, name):
    path = Path(target)
    if (not path.is_absolute() or path.name != name or path.parent.parent != temporary_directory / "openchamber"
            or not re.fullmatch(r"[0-9a-f]{16}", path.parent.name) or path.resolve() != path):
        raise ValueError("staging target escaped the owned OpenCode temporary directory")
    return path


def verify_prompt(body, captures):
    expected = [{"name": name, "uri": value["staging"]["uri"]} for name, value in captures.items()]
    if (body.get("text") != DRAFT or not isinstance(body.get("id"), str) or not body["id"].startswith("msg_")
            or not isinstance(body.get("files"), list) or len(body["files"]) != len(expected)
            or sorted(body["files"], key=lambda file: file.get("name", "")) != sorted(expected, key=lambda file: file["name"])):
        raise ValueError("sent prompt changed the captured text, names or file references")
    return body["id"]


class AttachmentProxy(FaultProxy):
    """Reuse the workflow proxy's loopback server and socket ownership.

    Only small fixture upload/prompt bodies are retained. One named upload is
    declined before forwarding; every other captured request reaches the server.
    No payload queue or application-state write is supplied by the driver.
    """

    def __init__(self, upstream, evidence):
        self.evidence, self.records, self.errors = evidence, [], []
        self.fail_name = TEXT_NAME
        # Session setup uses the fixture API directly. The base's prompt-loss
        # behavior is never used because prompt requests are handled below.
        super().__init__(upstream, "before", operation="create")

    def snapshot(self):
        with self.lock:
            return json.loads(json.dumps(self.records))

    def handle(self, handler):
        path = urllib.parse.urlsplit(handler.path).path
        if handler.command != "POST" or (path != WRITE_ROUTE and not path.endswith("/prompt")):
            super().handle(handler)
            return
        connection = None
        record = None
        try:
            if not handler.path.startswith("/") or handler.path.startswith("//") or handler.headers.get("Transfer-Encoding"):
                raise ValueError("captured request has unsupported framing")
            length = int(handler.headers.get("Content-Length", "-1"))
            if not 0 <= length <= 2 * 1024 * 1024:
                raise ValueError("captured fixture request is missing its bounded length")
            handler.connection.settimeout(5)
            body = handler.rfile.read(length)
            if len(body) != length:
                raise ValueError("captured request body is incomplete")
            target = urllib.parse.parse_qs(urllib.parse.urlsplit(handler.path).query).get("path", [])
            if path == WRITE_ROUTE and len(target) != 1:
                raise ValueError("attachment write has no unique target path")
            with self.lock:
                record = {"ordinal": len(self.records), "path": path, "kind": "write" if path == WRITE_ROUTE else "prompt",
                          "directory": handler.headers.get("x-opencode-directory"),
                          "directoryEncoding": handler.headers.get("x-opencode-directory-encoding"),
                          "contentType": handler.headers.get("Content-Type"), "status": None}
                if path == WRITE_ROUTE:
                    record.update(target=target[0], bytes=len(body), sha256=hashlib.sha256(body).hexdigest())
                    record["bodyFile"] = f"request-{record['ordinal']:03d}.bin"
                    (self.evidence / record["bodyFile"]).write_bytes(body)
                else:
                    record["body"] = json.loads(body)
                failed = path == WRITE_ROUTE and Path(target[0]).name == self.fail_name
                if failed:
                    self.fail_name = None
                record["injectedFailure"] = failed
                self.records.append(record)
            if failed:
                status, headers, data = 500, [("Content-Type", "application/json")], b'{"error":"injected attachment write failure"}'
            else:
                headers = {key: value for key, value in handler.headers.items()
                           if key.lower() not in ("connection", "transfer-encoding", "content-length")}
                connection = http.client.HTTPConnection(self.host, self.port, timeout=30)
                connection.request(handler.command, handler.path, body=body, headers=headers)
                response = connection.getresponse()
                status, headers, data = response.status, response.getheaders(), response.read()
            with self.lock:
                record["status"] = status
            handler.send_response(status)
            for key, value in headers:
                if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                    handler.send_header(key, value)
            handler.send_header("Content-Length", str(len(data)))
            handler.end_headers()
            handler.wfile.write(data)
        except Exception as error:
            with self.lock:
                self.errors.append(str(error))
                if record is not None:
                    record["error"] = str(error)
            handler.close_connection = True
        finally:
            if connection:
                connection.close()


def isolated_environment(state, server_url, output):
    env = virtual_display.child_environment(runner.native_env(state, server_url, "openchamber-dark"))
    for key in PROFILE_KEYS:
        path = Path(env[key])
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        if output not in path.resolve().parents or path.is_symlink() or path.stat().st_uid != os.getuid():
            raise RuntimeError("profile escaped the owned evidence directory")
        path.chmod(0o700)
    env["GDK_BACKEND"] = "x11"
    return env


@contextmanager
def controller_profile(output):
    """GTK's clipboard producer also uses an owned HOME and XDG profile."""
    env = isolated_environment(output / "controller", "http://127.0.0.1:1", output)
    keys = (*PROFILE_KEYS, "GDK_BACKEND")
    previous = {key: os.environ.get(key) for key in keys}
    os.environ.update({key: env[key] for key in keys})
    try:
        yield
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


class AttachmentInteraction:
    def __init__(self, fixture, proxy, args, identity, report, cleanup):
        self.fixture, self.proxy, self.args, self.identity = fixture, proxy, args, identity
        self.report, self.cleanup, self.clients = report, cleanup, []
        self.client = None
        self.clipboard = ClipboardProbe()
        self.env = isolated_environment(args.output / "native-state", proxy.url, args.output)
        self.journal = Path(self.env["XDG_CONFIG_HOME"]) / "openchamber-native/drafts-and-sends.json"
        self.inputs = args.output / "inputs"
        self.inputs.mkdir(mode=0o700)
        (self.inputs / TEXT_NAME).write_bytes(TEXT_BYTES)
        png.write(self.inputs / PNG_NAME, png.Image(2, 2, bytes([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255])))
        self.expected = {TEXT_NAME: TEXT_BYTES, PNG_NAME: (self.inputs / PNG_NAME).read_bytes()}
        self.report["sources"] = {name: byte_check(data, data) for name, data in self.expected.items()}

    def proof(self, name, detail=None):
        self.report["proofs"][name] = {"status": "passed", **(detail or {})}
        save(self.args.output / "report.json", self.report)

    def pointer(self, node):
        virtual_display.verify_owned_display()
        if node.get_component_iface() is None:
            raise Unobserved("pointer target has no AT-SPI Component geometry")
        rect = self.client._rect(node)
        if min(rect[2:]) <= 0 or rect[0] < 0 or rect[1] < 0 or rect[0] + rect[2] > 1281 or rect[1] + rect[3] > 801:
            raise ValueError("pointer target is outside the native content viewport")
        self.client._activate()
        x, y = self.client._screen_point(node)
        command(self.env, "xdotool", "mousemove", "--sync", str(x), str(y))
        if self.client._pointer() != (x, y):
            raise RuntimeError("pointer did not reach the target; no accessibility substitute is allowed")
        self.report.setdefault("pointerInputs", []).append({**text_node(node), "rect": rect, "point": [x, y]})
        command(self.env, "xdotool", "click", "1")

    def control(self, name, roles=BUTTON_ROLES):
        try:
            return until(f"AT-SPI control did not appear: {name}", lambda: named_control(self.client._walk(), name, roles), 20)
        except TimeoutError as error:
            raise Unobserved(str(error)) from error

    def composer(self):
        probe = object.__new__(Interaction)
        probe.client = self.client
        return probe.composer()

    def retained(self, focus=False):
        entry = self.composer()
        if entry is None:
            return False
        entry.clear_cache()
        return editable_text(entry) == DRAFT and (not focus or entry.get_state_set().contains(_atspi().StateType.FOCUSED))

    def row(self, name):
        return file_row(self.client._walk(), name)

    def capture(self, label, required=True):
        # Text and geometry are separate so a non-Component node cannot destroy
        # the original tree receipt or prevent a screenshot attempt.
        failures = []
        for suffix, read in (("tree.json", lambda: [text_node(node) for node in self.client._walk()]),
                             ("geometry.json", lambda: {name: guarded_geometry(self.client, row) for name in self.expected
                                                       if (row := self.row(name)) is not None}),
                             ("png", lambda: self.client.screenshot(self.args.output / f"{label}.png"))):
            try:
                value = read()
                if suffix != "png":
                    save(self.args.output / f"{label}-{suffix}", value)
            except Exception as error:
                self.report.setdefault("captureErrors", []).append({"capture": f"{label}-{suffix}", "error": str(error)})
                if suffix != "geometry.json":
                    failures.append(suffix)
        if required and failures:
            raise RuntimeError(f"checkpoint evidence could not be captured: {label}, {failures}")

    def launch(self, label):
        virtual_display.verify_owned_display()
        if binary_info(self.args.binary) != self.identity:
            raise RuntimeError("candidate changed before native launch")
        log = self.cleanup.enter_context((self.args.output / f"native-{label}.log").open("w"))
        client = NativeClient(self.args.binary, self.env, self.args.scale, 1280, 800, stderr=log)
        self.client = client
        self.clients.append(client)

        def stop():
            with self.fixture.interrupts.defer():
                if client.process is not None:
                    result = client.close() if client.process.poll() is None else {"exitCode": client.process.returncode}
                    self.report.setdefault("nativeCleanup", []).append({"pid": client.process.pid, **result})
                    if client.process.poll() is None:
                        raise RuntimeError("owned native process remained after cleanup")
        self.cleanup.callback(stop)
        previous = Path.cwd()
        try:
            os.chdir(self.fixture.project)
            with self.fixture.interrupts.defer():
                client.launch()
        finally:
            os.chdir(previous)
        client.wait_ready(45, connected=True)
        row = until("owned session did not appear", lambda: next((row for row in client.rows()
                    if row["key"].endswith(":session:" + self.session)), None), 30)
        self.pointer(client._find(row["key"]))
        until("another session was selected", lambda: any(row["selected"] and row["key"].endswith(":session:" + self.session)
              for row in client.rows()), 20)
        until("composer did not appear", self.composer, 20)

    def snapshot(self, label, expected, phases=None):
        def read():
            if not self.journal.exists():
                return None
            document = json.loads(self.journal.read_text())
            # Earlier durable snapshots remain valid while intake publishes its
            # files and target. Poll those states without accepting them as the
            # requested capture or hiding invalid byte/ownership metadata.
            if document.get("version") in (7, 8):
                drafts = [draft for draft in document.get("drafts", [])
                          if draft.get("owner", {}).get("target") == {"session": self.session}
                          and draft.get("owner", {}).get("directory") == str(self.fixture.project)]
                if not drafts:
                    return None
                if len(drafts) == 1:
                    draft = drafts[0]
                    files = draft.get("content", {}).get("owned", {}).get("files")
                    if draft.get("text") != DRAFT or isinstance(files, list) and sorted(file.get("name", "") for file in files) != sorted(expected):
                        return None
                    if isinstance(files, list) and isinstance(document.get("attachments"), list):
                        for file in files:
                            items = [item for item in document["attachments"] if item.get("id") == file.get("id")]
                            if not items or len(items) == 1 and items[0].get("staging", "missing") is None:
                                return None
            captures = draft_capture(document, self.session, self.fixture.project, expected)
            if phases and any(captures[name]["staging"]["phase"] != phase for name, phase in phases.items()):
                return None
            return captures
        captures = until("captured attachment journal did not reach its expected phases", read, 30)
        save(self.args.output / f"{label}-captures.json", captures)
        (self.args.output / f"{label}-journal.json").write_bytes(self.journal.read_bytes())
        for name, item in captures.items():
            spool = self.journal.parent / "attachment-spool" / f"blob-{item['id']}.bin"
            if spool.is_symlink() or spool.resolve() != spool:
                raise ValueError("spool path escaped its private profile")
            self.check_bytes(f"{label}-spool-{name}", spool.read_bytes(), expected[name])
        return captures

    def check_bytes(self, label, actual, expected):
        receipt = byte_check(actual, expected)
        self.report.setdefault("byteChecks", {})[label] = receipt
        if not receipt["matches"]:
            raise ValueError(f"attachment byte mismatch: {label}")

    def uploads(self, name):
        return [item for item in self.proxy.snapshot() if item["kind"] == "write" and Path(item["target"]).name == name]

    def verify_upload(self, name, count, captures):
        target = captures[name]["staging"]
        if target["temporary_directory"] != str(self.temporary):
            raise ValueError("captured staging temporary root changed")
        records = [item for item in self.uploads(name) if item["target"] == target["server_path"]]
        if len(records) != count:
            raise ValueError(f"unexpected upload attempt count for {name}: {len(records)}")
        for record in records:
            if (record["target"] != target["server_path"] or record["directory"] != urllib.parse.quote(str(self.fixture.project), safe="")
                    or record["directoryEncoding"] != "uri" or record["contentType"] != "application/octet-stream"):
                raise ValueError("attachment upload target, directory scope or MIME changed")
            self.check_bytes(f"request-{record['ordinal']}-{name}", (self.proxy.evidence / record["bodyFile"]).read_bytes(), self.expected[name])
        if records[-1]["status"] != 200:
            raise ValueError("last upload was not acknowledged by the server")
        path = owned_server_path(target["server_path"], self.temporary, name)
        self.check_bytes(f"server-{name}-{count}", path.read_bytes(), self.expected[name])

    def paste(self, names):
        self.pointer(self.composer())
        until("composer did not focus before file paste", lambda: self.retained(True), 20)
        paste_files(self.clipboard, [self.inputs / name for name in names], self.inputs,
                    lambda: command(self.env, "xdotool", "key", "--clearmodifiers", "ctrl+v"),
                    lambda: all(self.row(name) is not None for name in names))
        until("file-list paste changed the draft or composer focus", lambda: self.retained(True), 20)

    def run(self):
        self.session = self.fixture.create_session("stream-1200cps")["id"]
        self.report["sessionID"] = self.session
        info = self.fixture.get("/api/info")
        temporary = info.get("paths", {}).get("tmp")
        if not isinstance(temporary, str):
            raise Unobserved("server.info supplied no temporary-directory metadata")
        self.temporary = Path(temporary)
        if self.fixture.state / "tmp" not in (self.temporary, *self.temporary.parents) or self.temporary.resolve() != self.temporary:
            raise ValueError("server temporary root escaped the owned fixture")
        self.launch("first")
        self.pointer(self.composer())
        command(self.env, "xdotool", "type", "--clearmodifiers", DRAFT)
        until("synthetic draft was not entered exactly", lambda: self.retained(True), 20)
        self.capture("before-menu")
        portal = command(self.env, "busctl", "--user", "call", "org.freedesktop.DBus", "/org/freedesktop/DBus",
                         "org.freedesktop.DBus", "NameHasOwner", "s", "org.freedesktop.portal.Desktop").strip()
        if portal != "b false":
            raise Unobserved("private picker isolation cannot be verified because a portal already owns the private bus")
        self.pointer(self.control("Add attachment"))
        menu_item = self.control("Attach files", ("menu item",))
        menus = [node for node in self.client._walk() if node.get_role_name() == "menu"]
        if not menus or not any(node.get_state_set().contains(_atspi().StateType.FOCUSED) for menu in menus for node in children(menu)):
            raise ValueError("attachment menu did not own native focus")
        if not self.retained():
            raise ValueError("opening Attach discarded the draft")
        self.capture("attach-menu")
        self.pointer(menu_item)
        until("Attach files did not return focus and the exact draft after the private picker refusal", lambda: self.retained(True), 20)
        until("picker refusal was not reported", lambda: self.client._find("session-notice").get_name() == "Failed to attach file", 20)
        self.capture("picker-unavailable")
        self.report["picker"] = {"status": "unsupported", "selectionQualified": False,
                                 "reason": "Owned private D-Bus has no desktop portal; no picker service is activated."}
        self.proof("menu", {"draftRetained": True, "menuFocusObserved": True, "composerFocusRestored": True})

        self.paste([TEXT_NAME, PNG_NAME])
        captures = self.snapshot("failed-write", self.expected, {TEXT_NAME: "uncertain", PNG_NAME: "ready"})
        text_row = until("failed text attachment did not render", lambda: self.row(TEXT_NAME), 20)
        retry = named_control(children(text_row), "Retry attachment", BUTTON_ROLES)
        if retry is None:
            raise Unobserved("failed attachment exposes no named retry button")
        failed = self.uploads(TEXT_NAME)
        if len(failed) != 1 or not failed[0]["injectedFailure"] or failed[0]["status"] != 500:
            raise ValueError("expected exactly one injected text upload failure")
        failed_path = owned_server_path(failed[0]["target"], self.temporary, TEXT_NAME)
        if failed_path.exists():
            raise ValueError("declined upload unexpectedly wrote server bytes")
        self.verify_upload(PNG_NAME, 1, captures)
        for name in self.expected:
            if named_control(children(self.row(name)), "Remove " + name, BUTTON_ROLES) is None:
                raise Unobserved("attachment row exposes no named remove button")
        self.capture("failed-write")
        self.proof("clipboard-files", {"mime": "text/uri-list", "filenames": list(self.expected), "draftAndFocusRetained": True})
        self.pointer(retry)
        captures = self.snapshot("retried", self.expected, {name: "ready" for name in self.expected})
        until("ready attachment retained its retry control", lambda: named_control(children(self.row(TEXT_NAME)), "Retry attachment", BUTTON_ROLES) is None, 20)
        self.verify_upload(TEXT_NAME, 2, captures)
        self.verify_upload(PNG_NAME, 1, captures)
        until("retry changed the draft", self.retained, 20)
        self.capture("retried")
        self.proof("retry", {"sameTarget": True, "siblingUploadCount": 1})

        old_png = captures[PNG_NAME]
        self.pointer(named_control(children(self.row(PNG_NAME)), "Remove " + PNG_NAME, BUTTON_ROLES)
                     or self.control("Remove " + PNG_NAME))
        until("removed PNG remained rendered", lambda: self.row(PNG_NAME) is None, 20)
        self.snapshot("removed", {TEXT_NAME: self.expected[TEXT_NAME]}, {TEXT_NAME: "ready"})
        until("removal changed the draft", self.retained, 20)
        self.capture("removed")
        self.paste([PNG_NAME])
        captures = self.snapshot("before-restart", self.expected, {name: "ready" for name in self.expected})
        if captures[PNG_NAME]["id"] == old_png["id"]:
            raise ValueError("new PNG input reused its retired immutable capture")
        # The re-added file owns a new target. Verify its sole new upload and keep
        # the previous target's receipts separately, without deleting either.
        png_uploads = self.uploads(PNG_NAME)
        if len(png_uploads) != 2 or png_uploads[-1]["target"] != captures[PNG_NAME]["staging"]["server_path"]:
            raise ValueError("re-added PNG did not get exactly one new staging upload")
        self.verify_upload(PNG_NAME, 1, captures)
        self.proof("remove", {"removedID": old_png["id"], "replacementID": captures[PNG_NAME]["id"]})
        before_requests = self.proxy.snapshot()
        if len(before_requests) != 4 or any(item["kind"] == "prompt" for item in before_requests):
            raise ValueError("attachment intake submitted a prompt or repeated an upload")
        self.capture("before-restart")
        self.pointer(self.client._find("window-close"))
        self.client.process.wait(timeout=15)
        if self.client.process.returncode != 0:
            raise RuntimeError("normal native close did not exit cleanly")
        self.launch("restart")
        until("restart did not restore the exact composer draft", self.retained, 20)
        for name, item in captures.items():
            until("restart lost the rendered filename or capture identity", lambda name=name, item=item:
                  (row := self.row(name)) is not None and row.get_accessible_id() == "attachment-" + item["id"], 20)
        restored = self.snapshot("restart", self.expected, {name: "ready" for name in self.expected})
        if restored != captures or self.proxy.snapshot() != before_requests:
            raise ValueError("restart changed captured metadata or automatically uploaded/submitted")
        self.capture("restart")
        self.proof("restart-capture", {"exactCapturedDescriptors": True, "automaticUploadsOrPrompts": False})
        self.pointer(self.composer())
        self.pointer(self.client._find("composer-submit"))
        messages_path = f"/api/session/{urllib.parse.quote(self.session, safe='')}/message?limit=100"
        users = until("attachment prompt did not reach authoritative history", lambda: [message for message in self.fixture.data(messages_path, list)
                      if message.get("type") == "user"], 30)
        prompts = [item for item in self.proxy.snapshot() if item["kind"] == "prompt"]
        if len(prompts) != 1 or prompts[0]["path"] != f"/api/session/{self.session}/prompt" or prompts[0]["status"] != 200:
            raise ValueError("native send was not exactly one acknowledged prompt to the owned session")
        message_id = verify_prompt(prompts[0]["body"], captures)
        if len(users) != 1 or users[0].get("id") != message_id or users[0].get("text") != DRAFT:
            raise ValueError("server history duplicated or changed the owned prompt")
        files = users[0].get("files")
        if not isinstance(files, list) or any("name" not in file or "mime" not in file for file in files):
            raise Unobserved("authoritative message lacks attachment name/MIME metadata")
        if sorted((file["name"], file["mime"]) for file in files) != sorted((name, item["mediaType"]) for name, item in captures.items()):
            raise ValueError("server history changed the attachment names or media types")
        until("fixture turn did not settle", lambda: self.session not in self.fixture.data("/api/session/active", dict), 30)
        if self.proxy.snapshot()[:-1] != before_requests:
            raise ValueError("send repeated a completed upload")
        save(self.args.output / "messages.json", self.fixture.data(messages_path, list))
        self.capture("sent")
        self.proof("send-bytes", {"messageID": message_id, "prompts": 1, "serverFilesChecked": list(captures)})


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity, engine = binary_info(args.binary), binary_info(args.opencode_binary)
    report = {"status": "incomplete", "passed": False, "fullParity": False, "scope": "native-attachments-first-unit",
              "binary": identity, "opencodeBinary": engine, "scale": args.scale, "proofs": {}, "cleanupErrors": [],
              "limitations": ["Installed Electron reference is preserved and is not launched or compared.",
                              "No picker file selection, drop, image-only clipboard, IME or document-conversion qualification.",
                              "Restart covers completed draft captures and one normal close, not interrupted intake or whole-close parity.",
                              "No payload queue workflow or provider-side binary interpretation qualification.",
                              "Evidence and synthetic fixture files are retained; no fixture root is reclaimed."]}
    interaction = proxy = None
    previous = None
    root = Path("/tmp/opencode") / f"native-attachments-fixture-{uuid.uuid4().hex}"
    report["fixtureRoot"] = str(root)
    save(args.output / "isolation.json", {"display": os.environ["DISPLAY"], "xvfbPid": int(os.environ[virtual_display.XVFB_PID]),
         "runtimeMode": oct(stat.S_IMODE(Path(os.environ["XDG_RUNTIME_DIR"]).stat().st_mode)),
         "privateBusVerified": True, "waylandVariablesAbsent": True})
    try:
        previous = accessibility_enabled()
        set_accessibility(True)
        with Interrupts() as interrupts, controller_profile(args.output), ExitStack() as cleanup:
            virtual_display.verify_owned_display()
            fixture = cleanup.enter_context(HostFixture(root, args.output / "fixture", args, interrupts))
            proxy_dir = args.output / "requests"
            proxy_dir.mkdir(mode=0o700)
            proxy = AttachmentProxy(fixture.url, proxy_dir)

            def stop_proxy():
                with interrupts.defer():
                    proxy.stop()
                    report["proxyCleanup"] = {"threadStopped": not proxy.thread.is_alive()}
                    if proxy.thread.is_alive():
                        raise RuntimeError("owned attachment proxy remained after cleanup")
            cleanup.callback(stop_proxy)
            interaction = AttachmentInteraction(fixture, proxy, args, identity, report, cleanup)
            try:
                interaction.run()
            except BaseException:
                # Retain the original failure before any cleanup can fail too.
                (args.output / "failure.txt").write_text(traceback.format_exc())
                if interaction.client and interaction.client.process and interaction.client.process.poll() is None:
                    interaction.capture("failed", required=False)
                raise
    except (Exception, KeyboardInterrupt) as error:
        if not (args.output / "failure.txt").exists():
            (args.output / "failure.txt").write_text(traceback.format_exc())
        else:
            (args.output / "exit-failure.txt").write_text(traceback.format_exc())
        report["error"] = str(error)
        report["proofs"]["run"] = {"status": "unobserved" if isinstance(error, Unobserved) else "failed", "error": str(error)}
        report["interrupted"] = isinstance(error, (RunInterrupted, KeyboardInterrupt))
    finally:
        if previous is not None:
            try:
                set_accessibility(previous)
            except Exception as error:
                report["cleanupErrors"].append(f"private accessibility restoration: {error}")
        if interaction:
            report["nativeProcesses"] = [{"pid": client.process.pid, "exitCode": client.process.poll()}
                                         for client in interaction.clients if client.process is not None]
            report["inputSubstitutions"] = [item for client in interaction.clients for item in client.substitutions]
            if report["inputSubstitutions"] or any(item["exitCode"] is None for item in report["nativeProcesses"]):
                report["cleanupErrors"].append("native process remained or pointer input used an accessibility substitution")
        if proxy:
            save(args.output / "requests.json", proxy.snapshot())
            report["proxyErrors"] = proxy.errors
            if proxy.errors:
                report["proofs"]["proxy"] = {"status": "failed", "errors": proxy.errors}
        cleanup_receipt = args.output / "fixture/cleanup.json"
        if cleanup_receipt.exists():
            report["fixtureCleanup"] = json.loads(cleanup_receipt.read_text())
            if report["fixtureCleanup"]["remainingPids"]:
                report["cleanupErrors"].append("owned fixture processes remain")
        else:
            report["cleanupErrors"].append("fixture cleanup receipt is unavailable")
        try:
            if binary_info(args.binary) != identity or binary_info(args.opencode_binary) != engine:
                report["cleanupErrors"].append("native or engine binary changed during qualification")
        except OSError as error:
            report["cleanupErrors"].append(str(error))
        report["status"] = final_status(report["proofs"], report["cleanupErrors"])
        report["passed"] = report["status"] == "passed"
        save(args.output / "report.json", report)
    print(f"{report['status']}: {args.output / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--dist", type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument("--opencode-binary", type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument("--scale", type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    args = parser.parse_args()
    for name in ("binary", "dist", "opencode_binary"):
        setattr(args, name, getattr(args, name).resolve())
    args.output = args.output.absolute()
    try:
        validate_output(args.output)
    except ValueError as error:
        parser.error(str(error))
    for executable in (args.binary, args.opencode_binary):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            parser.error(f"executable is missing: {executable}")
    if not virtual_display.inside():
        if problem := virtual_display.available():
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == "__main__":
    raise SystemExit(main())
