#!/usr/bin/env python3
"""Desktop notifications and dictation on an owned server and an owned Xvfb.

Notifications: a stub `org.freedesktop.Notifications` on the run's private
session bus records every `Notify` call. The Notifications page's test
button, a template saved from the page, a finished turn's notification with
that template, a click (the stub emits `ActionInvoked`) that opens the
notifying session from another one, and no notification once the page turned
them off.

Dictation: an ALSA `file` plugin plays a fixed tone as the microphone and the
server's OpenAI-compatible transcription points at a loopback fixture. The
mic, recording, Insert, a failed transcription with Retry, Cancel, and the mic
gone once Voice settings turn voice input off. The local speech models are
not used: they are a download of hundreds of megabytes.

Native-only runtime evidence, not matched-client parity.
"""

import argparse
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
from pathlib import Path
import struct
import sys
import threading
import time
import traceback
import urllib.request
import uuid

import runner
import virtual_display
from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_files import Session
from native_hosts import HostFixture
from native_interactions import Interrupts, until
from native_platform import binary_info

SETTINGS = '/api/config/settings?surface=desktop'
NOTIFICATIONS_XML = """
<node>
  <interface name="org.freedesktop.Notifications">
    <method name="Notify">
      <arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/>
      <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/>
      <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/>
    </method>
    <method name="CloseNotification"><arg type="u" direction="in"/></method>
    <method name="GetCapabilities"><arg type="as" direction="out"/></method>
    <method name="GetServerInformation">
      <arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/>
    </method>
    <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
    <signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
  </interface>
</node>
"""


class NotificationService:
    """Owns `org.freedesktop.Notifications` on the private bus."""

    def __init__(self):
        import gi
        gi.require_version('Gio', '2.0')
        from gi.repository import Gio, GLib
        self.Gio, self.GLib = Gio, GLib
        self.calls = []
        self.lock = threading.Lock()
        self.owned = threading.Event()
        self.loop = GLib.MainLoop()
        self.connection = None
        threading.Thread(target=self._serve, daemon=True).start()
        if not self.owned.wait(10):
            raise RuntimeError('the stub notification service could not own its name')

    def _serve(self):
        Gio, GLib = self.Gio, self.GLib
        self.connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        info = Gio.DBusNodeInfo.new_for_xml(NOTIFICATIONS_XML).interfaces[0]

        def call(_connection, sender, _path, _interface, method, parameters, invocation):
            if method == 'Notify':
                app, _replaces, _icon, summary, body, actions, _hints, _timeout = parameters.unpack()
                with self.lock:
                    self.calls.append({'app': app, 'summary': summary, 'body': body, 'actions': actions,
                                       'sender': sender})
                    identifier = len(self.calls)
                invocation.return_value(GLib.Variant('(u)', (identifier,)))
            elif method == 'GetCapabilities':
                invocation.return_value(GLib.Variant('(as)', (['actions', 'body'],)))
            elif method == 'GetServerInformation':
                invocation.return_value(GLib.Variant('(ssss)', ('stub', 'parity', '1', '1.2')))
            else:
                invocation.return_value(None)

        self.connection.register_object('/org/freedesktop/Notifications', info, call, None, None)
        Gio.bus_own_name_on_connection(self.connection, 'org.freedesktop.Notifications',
                                       Gio.BusNameOwnerFlags.NONE, lambda *_: self.owned.set(), None)
        self.loop.run()

    def snapshot(self):
        with self.lock:
            return list(self.calls)

    def click(self, identifier):
        """What a notification daemon sends when its notification is clicked."""
        call = self.calls[identifier - 1]
        self.connection.emit_signal(call['sender'], '/org/freedesktop/Notifications',
                                    'org.freedesktop.Notifications', 'ActionInvoked',
                                    self.GLib.Variant('(us)', (identifier, 'default')))

    def stop(self):
        self.loop.quit()


class Transcriber:
    """A loopback OpenAI-compatible transcription endpoint."""

    TEXT = 'dictated through the fixture'

    def __init__(self):
        self.requests = []
        self.fail = False
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get('Content-Length') or 0))
                owner.requests.append({'path': self.path, 'bytes': len(body), 'wav': b'RIFF' in body,
                                       'failed': owner.fail})
                status, payload = (500, {'error': {'message': 'fixture failure'}}) if owner.fail else (
                    200, {'text': owner.TEXT})
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f'http://127.0.0.1:{self.server.server_address[1]}/v1'

    def stop(self):
        self.server.shutdown()


def fake_microphone(folder):
    """An ALSA configuration whose default capture device plays a tone."""
    tone = folder / 'tone.raw'
    tone.write_bytes(b''.join(struct.pack('<h', int(8000 * math.sin(2 * math.pi * 440 * i / 16000)))
                              for i in range(16000 * 60)))
    config = folder / 'asound.conf'
    config.write_text(f'pcm.!default {{\n type file\n slave.pcm "null"\n file "/dev/null"\n'
                      f' infile "{tone}"\n format "raw"\n}}\npcm.null {{ type null }}\n')
    return config


def put(fixture, values):
    request = urllib.request.Request(fixture.url + SETTINGS, data=json.dumps(values).encode(),
                                     headers={'Content-Type': 'application/json'}, method='PUT')
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'checks': [],
              'limitations': ['Native-only focused outcomes; no matched-client parity claim.',
                              'Microphone permission denial is not reproduced: ALSA has no consent prompt.']}
    previous = accessibility_enabled()
    clients = []
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            service = NotificationService()
            cleanup.callback(service.stop)
            transcriber = Transcriber()
            cleanup.callback(transcriber.stop)
            fixture = cleanup.enter_context(HostFixture(
                Path('/tmp/opencode') / f'notify-voice-{uuid.uuid4().hex}', args.output / 'server', args, interrupts))
            first = fixture.create_session('stream-1200cps')
            second = fixture.create_session('stream-1200cps')
            put(fixture, {
                'nativeNotificationsEnabled': True, 'notificationMode': 'always', 'notifyOnCompletion': True,
                'dictationEnabled': True, 'dictationAutoSend': False, 'sttProvider': 'openai-compatible',
                'sttServerUrl': transcriber.url, 'sttModel': 'fixture-model',
            })
            alsa = fake_microphone(args.output)
            state = args.output / 'native-state'
            env = runner.native_env(state, fixture.url, 'openchamber-dark')
            env['ALSA_CONFIG_PATH'] = str(alsa)
            for folder in runner.state_dirs(state).values():
                Path(folder).mkdir(parents=True, exist_ok=True)
            log = cleanup.enter_context((args.output / 'native.log').open('w'))
            client = NativeClient(args.binary, virtual_display.child_environment(env), args.scale, 1280, 800,
                                  stderr=log)
            cleanup.callback(client.close)
            clients.append(client)
            client.launch()
            client.wait_ready(45, connected=True)
            session = object.__new__(Session)
            session.client, session.env, session.project, session.args = client, env, fixture.project, args

            def record(name):
                client.screenshot(args.output / f'{name}.png')
                report['checks'].append(name)

            def select(sid):
                def choose():
                    rows = [row for row in client.rows() if row['key'].endswith(':session:' + sid)]
                    if len(rows) != 1:
                        return False
                    client.click(rows[0]['key'])
                    return True
                until('the session row did not arrive', choose, 45)
                until('the session was not selected', lambda: is_selected(sid), 20)

            def is_selected(sid):
                return any(row['selected'] and row['key'].endswith(':session:' + sid) for row in client.rows())

            def saved(key):
                return fixture.get(SETTINGS).get(key)

            # Settings: the test notification, then a custom completion title.
            session.wait_id('settings')
            client.activate('settings')
            session.wait_id('settings-dialog')
            client.click('settings-nav-notifications')
            session.wait_id('notifications-delivery')
            assert session.has_state('notifications-delivery', 'CHECKED'), 'saved switch not shown'
            client.click('notifications-test')
            until('the test notification did not reach the service', lambda: any(
                call['summary'] == 'Test notification' for call in service.snapshot()), 15)
            record('test-notification-delivered')
            # The field is below the fold; focus moves it into view.
            if not client._find('notifications-template-completion-title').get_component_iface().grab_focus():
                raise RuntimeError('the template field did not take focus')
            time.sleep(0.5)
            session.type('Done in {session_name}')
            time.sleep(1)
            client.screenshot(args.output / 'template-typed.png')
            until('the template was not saved', lambda: (saved('notificationTemplates') or {}).get(
                'completion', {}).get('title') == 'Done in {session_name}', 15)
            record('template-saved')
            session.keys('Escape')
            session.wait_gone('settings-dialog')

            # A finished turn notifies with the template; a click opens it.
            select(first['id'])
            client.screenshot(args.output / 'first-selected.png')
            session.type('Write one short line.')
            session.keys('Return')
            time.sleep(3)
            client.screenshot(args.output / 'turn-sent.png')
            until('the finished turn did not notify', lambda: any(
                call['summary'].startswith('Done in ') for call in service.snapshot()), 120)
            notified = next(index + 1 for index, call in enumerate(service.snapshot())
                            if call['summary'].startswith('Done in '))
            report['completionNotification'] = service.snapshot()[notified - 1]
            assert service.snapshot()[notified - 1]['actions'][0] == 'default', 'no click action'
            select(second['id'])
            service.click(notified)
            until('the click did not open the notifying session', lambda: is_selected(first['id']), 20)
            record('click-opens-owning-session')

            # Dictation: record, Insert, the transcript in the composer.
            select(second['id'])
            session.wait_id('composer-dictation')
            client.click('composer-dictation')
            session.wait_id('dictation-overlay')
            session.wait_id('dictation-insert')
            time.sleep(2.5)
            record('dictation-recording')
            client.click('dictation-insert')
            session.wait_gone('dictation-overlay', 30)
            until('the transcript was not inserted', lambda: Transcriber.TEXT in (client.focused_text() or ''), 15)
            report['transcriptionRequests'] = list(transcriber.requests)
            assert transcriber.requests and transcriber.requests[-1]['wav'], 'no audio reached the endpoint'
            record('dictation-inserted')
            session.keys('ctrl+a', 'BackSpace')

            # A failed transcription keeps the recording; Retry sends it again.
            transcriber.fail = True
            client.click('composer-dictation')
            session.wait_id('dictation-insert')
            time.sleep(1.5)
            client.click('dictation-insert')
            session.wait_id('dictation-retry', 30)
            record('dictation-failed')
            transcriber.fail = False
            client.click('dictation-retry')
            session.wait_gone('dictation-overlay', 30)
            until('retry did not insert the transcript', lambda: Transcriber.TEXT in (client.focused_text() or ''), 15)
            record('dictation-retried')
            session.keys('ctrl+a', 'BackSpace')

            # Cancel leaves the composer and the endpoint untouched.
            before = len(transcriber.requests)
            client.click('composer-dictation')
            session.wait_id('dictation-cancel')
            time.sleep(1)
            client.click('dictation-cancel')
            session.wait_gone('dictation-overlay', 10)
            time.sleep(1)
            assert len(transcriber.requests) == before, 'a cancelled dictation was transcribed'
            assert Transcriber.TEXT not in (client.focused_text() or ''), 'a cancelled dictation inserted text'
            record('dictation-cancelled')

            # Settings turn voice input and notifications off.
            client.activate('settings')
            session.wait_id('settings-dialog')
            client.click('settings-nav-voice')
            session.wait_id('voice-speech-recognition')
            client.click('voice-speech-recognition')
            until('voice input stayed on', lambda: saved('dictationEnabled') is False, 15)
            client.click('settings-nav-notifications')
            session.wait_id('notifications-delivery')
            client.click('notifications-delivery')
            until('notifications stayed on', lambda: saved('nativeNotificationsEnabled') is False, 15)
            session.keys('Escape')
            session.wait_gone('settings-dialog')
            until('the mic stayed while voice input is off', lambda: not session.has('composer-dictation'), 15)
            record('dictation-disabled')
            count = len(service.snapshot())
            select(first['id'])
            session.type('Another short line.')
            session.keys('Return')
            time.sleep(20)
            assert len(service.snapshot()) == count, 'a notification arrived while turned off'
            record('notifications-off-respected')
            report['notifications'] = service.snapshot()
            report['inputSubstitutions'] = client.substitutions
        report['passed'] = True
    except Exception as error:
        traceback.print_exc()
        report['error'] = str(error)
        if clients:
            try:
                clients[-1].screenshot(args.output / 'failed.png')
            except Exception as screenshot_error:
                report['screenshotError'] = repr(screenshot_error)
    finally:
        set_accessibility(previous)
        report['nativeProcesses'] = [{'pid': client.process.pid, 'exitCode': client.process.poll()}
                                     for client in clients if client.process]
        if binary_info(args.binary) != identity or any(item['exitCode'] is None for item in report['nativeProcesses']):
            report.update(passed=False, error=report.get('error') or 'binary changed or a native process survived')
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--dist', type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument('--opencode-binary', type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--scale', type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    args = parser.parse_args()
    for key in ('binary', 'dist', 'opencode_binary', 'output'):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path('/tmp/opencode') or args.output.exists():
        parser.error('--output must be new and directly under /tmp/opencode')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
