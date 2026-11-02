#!/usr/bin/env python3
"""Focused General Files/Terminal outcomes on owned servers and an owned Xvfb.

This is native-only runtime evidence, not full General or visual parity.
"""

import argparse
from contextlib import ExitStack
import io
import json
import os
from pathlib import Path
import shlex
import sys
import threading
import time
import traceback
import urllib.parse
import urllib.request
import uuid

import runner
import virtual_display
from native_adapter import NativeClient, _atspi, accessibility_enabled, set_accessibility
from native_files import Session
from native_hosts import HostFixture
from native_interactions import Interrupts, until
from native_platform import binary_info, command, wait_for
from native_terminal import TerminalProxy

SETTINGS = '/api/config/settings'
FIELDS = {'autoSaveEnabled': bool, 'terminalCloseOnExit': bool, 'terminalShell': str}


def settings_fields(value):
    """Require exact field types; absent and malformed values are unknown."""
    if not isinstance(value, dict):
        raise ValueError('settings response must be an object')
    return {key: value[key] for key, kind in FIELDS.items() if type(value.get(key)) is kind}


def expected_shell_executable(shell_id, shells_file=Path('/etc/shells')):
    """Resolve a configured Bash/Zsh executable for the owned HostFixture."""
    if shell_id not in ('bash', 'zsh'):
        raise ValueError('process proof requires an explicit Bash or Zsh selection')
    # isolated-server.mjs --selections sets SHELL=/bin/sh without a terminal
    # override. For Bash/Zsh, terminal/shells.js therefore checks /etc/shells
    # before PATH. The public shell list deliberately omits the executable.
    for line in shells_file.read_text().splitlines():
        executable = Path(line.strip())
        if (executable.is_absolute() and executable.name == shell_id
                and executable.is_file() and os.access(executable, os.X_OK)):
            return executable.resolve(strict=True)
    # The server's augmented PATH is private. A PATH-only shell has no verified
    # executable expectation for this fixture's process proof.
    raise RuntimeError(f'process proof requires an executable {shell_id} entry in {shells_file}')


class SettingsFaults(TerminalProxy):
    """Reuse terminal transport, adding bounded settings-only fault gates."""

    def __init__(self, upstream):
        self.read_failure = threading.Event()
        self.hold_reads = threading.Event()
        self.hold_writes = threading.Event()
        self.entered_write = threading.Event()
        self.fail_field = None
        self.settings_requests = []
        super().__init__(upstream)

    def decision(self, method, body):
        with self.lock:
            self.settings_requests.append({'method': method, 'body': body})
            if method == 'GET' and self.read_failure.is_set():
                return 503
            if method == 'PUT' and self.fail_field and self.fail_field in body:
                self.fail_field = None
                return 500
        return None

    def handle(self, handler):
        if urllib.parse.urlsplit(handler.path).path != SETTINGS:
            return super().handle(handler)
        length = int(handler.headers.get('Content-Length', 0))
        raw = handler.rfile.read(length) if length else b''
        body = json.loads(raw) if raw else {}
        gate = self.hold_reads if handler.command == 'GET' else self.hold_writes
        if handler.command == 'PUT' and gate.is_set():
            self.entered_write.set()
        deadline = time.monotonic() + 25
        while gate.is_set() and not self.stopped.is_set() and time.monotonic() < deadline:
            self.stopped.wait(0.05)
        status = self.decision(handler.command, body)
        if gate.is_set() or self.stopped.is_set():
            status = 503
        if status:
            data = b'{"error":"Injected General settings failure"}'
            handler.send_response(status)
            handler.send_header('Content-Type', 'application/json')
            handler.send_header('Content-Length', str(len(data)))
            handler.end_headers()
            try:
                handler.wfile.write(data)
            except OSError:
                handler.close_connection = True
            return
        original = handler.rfile
        try:
            handler.rfile = io.BytesIO(raw)
            super().handle(handler)
        finally:
            handler.rfile = original

    def stop(self):
        self.hold_reads.clear()
        self.hold_writes.clear()
        super().stop()


def put(fixture, values):
    request = urllib.request.Request(fixture.url + SETTINGS, data=json.dumps(values).encode(),
                                     headers={'Content-Type': 'application/json'}, method='PUT')
    with urllib.request.urlopen(request, timeout=10) as response:
        return settings_fields(json.load(response))


def keyboard_activate(session, key):
    """Acknowledge asynchronous native focus before sending the activation key."""
    session.client._activate()
    if not session.client._find(key).get_component_iface().grab_focus():
        raise RuntimeError(f'{key} refused keyboard focus')

    def focused():
        node = session.client._find(key)
        node.clear_cache()
        return node.get_state_set().contains(_atspi().StateType.FOCUSED)

    until(f'{key} did not receive keyboard focus', focused, 5)
    session.keys('space')


def open_shell_picker(session):
    def node():
        return session.node_named('Select terminal shell', roles=('combo box',))

    wait_for('Terminal shell combobox did not appear', node)
    target = node()
    session.click_node(target)
    if session.client._pointer() != session.client._screen_point(target):
        raise RuntimeError('private pointer did not reach the terminal shell combobox')

    def expanded():
        target = node()
        target.clear_cache()
        return target.get_state_set().contains(_atspi().StateType.EXPANDED)

    until('Terminal shell combobox did not open', expanded, 10)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'checks': [],
              'limitations': ['Native-only focused outcomes; no full General or matched-client parity claim.']}
    previous = accessibility_enabled()
    clients = []
    proxies = []
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixtures = [cleanup.enter_context(HostFixture(Path('/tmp/opencode') / f'general-{uuid.uuid4().hex}',
                        args.output / name, args, interrupts)) for name in ('alpha', 'beta')]
            first, second = fixtures
            sessions = []
            for fixture in fixtures:
                sessions.append(fixture.create_session('stream-1200cps'))
                put(fixture, {'autoSaveEnabled': True, 'terminalCloseOnExit': True, 'terminalShell': 'auto'})
                proxy = SettingsFaults(fixture.url)
                proxies.append(proxy)
                cleanup.callback(proxy.stop)
            proxy = proxies[0]
            proxy.hold_reads.set()
            state = args.output / 'native-state'
            env = runner.native_env(state, proxy.url, 'openchamber-dark')
            env.pop('OPENCHAMBER_NATIVE_SERVER', None)
            env.pop('OPENCHAMBER_NATIVE_PASSWORD', None)
            for folder in runner.state_dirs(state).values():
                Path(folder).mkdir(parents=True, exist_ok=True)
            profile = state / 'config/openchamber-native/hosts.json'
            profile.parent.mkdir(parents=True, exist_ok=True)
            profile.write_text(json.dumps({'version': 1, 'revision': 1, 'defaultHost': 'host_alpha',
                'hosts': [{'id': 'host_' + name, 'label': name, 'url': item.url}
                          for name, item in zip(('alpha', 'beta'), proxies)]}))
            target = first.project / 'general.txt'
            target.write_text('original\n')
            log = cleanup.enter_context((args.output / 'native.log').open('w'))
            virtual_display.verify_owned_display()
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            def capture_last():
                if client.process is not None and client.process.poll() is None:
                    try:
                        client.screenshot(args.output / 'last-state.png')
                    except Exception:
                        pass
            cleanup.callback(capture_last)
            clients.append(client)
            client.launch()
            client.wait_ready(45, connected=True)
            session = object.__new__(Session)
            session.client, session.env, session.project, session.args = client, env, first.project, args

            def activate(key, mode='a11y'):
                session.wait_id(key)
                if mode == 'keyboard':
                    keyboard_activate(session, key)
                else:
                    client.activate(key)
                if key == 'settings':
                    session.wait_id('general-auto-save')

            def record(name):
                client.screenshot(args.output / f'{name}.png')
                report['checks'].append(name)

            def observed(fixture, key, value):
                until(f'{key} did not persist as {value!r}', lambda: settings_fields(fixture.get(SETTINGS)).get(key) == value)

            def checkbox(key, value, mode='a11y'):
                if session.has_state(key, 'CHECKED') != value:
                    activate(key, mode)
                until(f'{key} did not retain its choice', lambda: session.has_state(key, 'CHECKED') == value)

            activate('settings', 'keyboard')
            session.wait_label('settings-current-page', lambda label: label == 'General', 'footer did not open General')
            record('footer-general-initial')
            # An explicit choice made while reads are held must survive a failed read.
            checkbox('general-auto-save', False, 'keyboard')
            observed(first, 'autoSaveEnabled', False)
            proxy.read_failure.set()
            proxy.hold_reads.clear()
            session.wait_id('general-retry')
            assert not session.has_state('general-auto-save', 'CHECKED')
            record('read-failure-retains-explicit-choice')
            proxy.read_failure.clear()
            activate('general-retry')
            session.wait_gone('general-retry')
            session.keys('Escape')
            session.wait_gone('settings-dialog')
            owned_row = until('Alpha session did not arrive', lambda: next((row for row in client.rows()
                if row['key'].endswith(':session:' + sessions[0]['id'])), None))
            client.click(owned_row['key'])
            if not runner.wait_selected(client, owned_row['key']):
                raise RuntimeError('Alpha selected another session')
            client.click('rail-files')
            row = f'files-row:{target}'
            session.wait_id(row)
            client.click(row)
            wait_for('owned file did not load', lambda: session.editor_text('general.txt') == 'original\n')
            session.click_node(session.node_named('general.txt'))
            session.keys('ctrl+End')
            session.type('manual')
            time.sleep(3)
            assert target.read_text() == 'original\n', 'disabled autosave wrote the file'
            session.keys('ctrl+s')
            until('explicit Save failed with autosave off', lambda: target.read_text() == 'original\nmanual')
            record('disabled-autosave-explicit-save')
            activate('settings')
            checkbox('general-auto-save', True)
            observed(first, 'autoSaveEnabled', True)
            session.keys('Escape')
            session.wait_gone('settings-dialog')
            session.click_node(session.node_named('general.txt'))
            session.keys('ctrl+End')
            session.type(' auto')
            until('reenabled autosave failed', lambda: target.read_text() == 'original\nmanual auto')
            record('reenabled-autosave')

            activate('settings')
            proxy.fail_field = 'autoSaveEnabled'
            checkbox('general-auto-save', False)
            session.wait_id('general-retry')
            assert settings_fields(first.get(SETTINGS))['autoSaveEnabled'] is True
            checkbox('general-terminal-close-on-exit', False, 'keyboard')
            observed(first, 'terminalCloseOnExit', False)
            assert not session.has_state('general-auto-save', 'CHECKED')
            session.wait_id('general-retry')
            record('failed-save-retains-choice-independent-field-saves')
            activate('general-retry')
            observed(first, 'autoSaveEnabled', False)
            session.wait_gone('general-retry')
            record('retry-persists')

            shells = first.get('/api/terminal/shells')
            candidate = next(item for item in shells if item['id'] in ('bash', 'zsh'))
            expected_executable = expected_shell_executable(candidate['id'])
            open_shell_picker(session)
            index = next(index for index, item in enumerate([item for item in shells if item['id'] != 'auto'], 1)
                         if item['id'] == candidate['id'])
            session.keys(*(['Down'] * index), 'Return')
            observed(first, 'terminalShell', candidate['id'])
            open_shell_picker(session)
            session.keys(*(['ctrl+p'] * index), 'Return')
            observed(first, 'terminalShell', 'auto')
            open_shell_picker(session)
            session.keys(*(['ctrl+n'] * index), 'Return')
            observed(first, 'terminalShell', candidate['id'])
            record('shell-picker-arrow-and-ctrl-n-p-navigation')
            session.keys('Escape')
            session.wait_gone('settings-dialog')

            def terminals():
                return first.get('/api/terminal/sessions?directory=' + urllib.parse.quote(str(first.project), safe=''))['sessions']

            def shell(line):
                session.wait_id('terminal-surface')
                session.click_node(client._find('terminal-surface'))
                command(env, 'xdotool', 'type', '--delay', '10', line)
                session.keys('Return')

            session.keys('ctrl+j')
            until('terminal was not created', lambda: proxy.terminal_commands('POST', '/create'))
            assert proxy.terminal_commands('POST', '/create')[-1]['body']['shell'] == candidate['id']
            proof = first.project / 'actual-shell.txt'
            shell('readlink /proc/$$/exe > ' + shlex.quote(str(proof)))
            until('actual shell did not write its executable', lambda: proof.exists() and proof.read_text().strip())
            actual_executable = Path(proof.read_text().strip()).resolve(strict=True)
            assert actual_executable == expected_executable, (
                f'selected {candidate["id"]} ran {actual_executable}, expected {expected_executable}')
            report['shellProcess'] = {'id': candidate['id'], 'expectedExecutable': str(expected_executable),
                                      'actualExecutable': str(actual_executable)}
            record('shell-persists-and-process-uses-selection')
            shell('exit 3')
            until('server terminal did not exit', lambda: any(item['status'] == 'exited' for item in terminals()))
            session.wait_id('terminal-tab-1')
            record('close-on-exit-false-retains-tab')
            activate('settings')
            checkbox('general-terminal-close-on-exit', True)
            observed(first, 'terminalCloseOnExit', True)
            session.keys('Escape')
            client.click('terminal-new-tab')
            until('new terminal did not run', lambda: any(item['status'] == 'running' for item in terminals()))
            deletes = len(proxy.terminal_commands('DELETE'))
            shell('exit 4')
            until('close-on-exit true sent no delete', lambda: len(proxy.terminal_commands('DELETE')) > deletes)
            session.wait_gone('terminal-tab-2')
            record('close-on-exit-true-closes-tab')

            activate('settings')
            proxy.hold_writes.set()
            checkbox('general-auto-save', True)
            until('settings write did not enter held target', proxy.entered_write.is_set)
            session.keys('Escape')
            activate('instance-switcher')
            activate('host-connect:host_beta')
            until('Beta session did not arrive', lambda: any(row['key'].endswith(':session:' + sessions[1]['id']) for row in client.rows()), 30)
            proxy.hold_writes.clear()
            observed(first, 'autoSaveEnabled', True)
            # Beta starts true. A false value is chosen only after the switch.
            activate('settings')
            checkbox('general-auto-save', False)
            observed(second, 'autoSaveEnabled', False)
            assert settings_fields(first.get(SETTINGS))['autoSaveEnabled'] is True
            record('held-write-keeps-captured-runtime-target')
            report['inputSubstitutions'] = client.substitutions
            if client.substitutions:
                raise RuntimeError('pointer input needed accessibility substitution')
        report['passed'] = True
    except Exception as error:
        traceback.print_exc()
        report['error'] = str(error)
        if clients:
            try:
                clients[-1].screenshot(args.output / 'failed.png')
            except Exception:
                pass
    finally:
        set_accessibility(previous)
        report['requests'] = [proxy.settings_requests for proxy in proxies]
        report['inputSubstitutions'] = [item for client in clients for item in client.substitutions]
        report['nativeProcesses'] = [{'pid': client.process.pid, 'exitCode': client.process.poll()}
                                    for client in clients if client.process]
        if binary_info(args.binary) != identity or any(item['exitCode'] is None for item in report['nativeProcesses']):
            report.update(passed=False, error='binary changed or a native process survived cleanup')
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
