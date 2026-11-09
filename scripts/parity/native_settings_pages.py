#!/usr/bin/env python3
"""Chat and Sessions Settings pages on an owned server and an owned Xvfb.

Navigation by pointer, search to a focused control, server-backed saves, an
injected failed save with Retry, a disabled choice, and the saved values after
a relaunch. Native-only runtime evidence, not matched-client parity.
"""

import argparse
from contextlib import ExitStack, contextmanager
import json
from pathlib import Path
import sys
import traceback
import urllib.request
import uuid

import runner
import virtual_display
from native_adapter import NativeClient, _atspi, accessibility_enabled, set_accessibility
from native_files import Session
from native_general_settings import SETTINGS, SettingsFaults
from native_hosts import HostFixture
from native_interactions import Interrupts, until
from native_platform import binary_info, command, wait_for

# Native writes as the desktop surface; several of these keys are per surface.
DESKTOP = SETTINGS + '?surface=desktop'
INITIAL = {
    'promptNavigatorEnabled': True,
    'followUpBehavior': 'queue',
    'draftStartersVisible': True,
    'permissionDefaultMode': 'ask',
    'sidebarShowChatsSection': True,
    'sessionWorkEnabled': True,
    'showDeletionDialog': True,
}


def put(fixture, values):
    request = urllib.request.Request(fixture.url + DESKTOP, data=json.dumps(values).encode(),
                                     headers={'Content-Type': 'application/json'}, method='PUT')
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)


def click_visible_deletion(client):
    """Wheel over Sessions content until the whole deletion control is visible."""
    virtual_display.verify_owned_display()
    api = _atspi()

    def bounds(target):
        node = client._find(target)
        node.clear_cache()
        return node.get_extents(api.CoordType.WINDOW)

    client._activate()
    for wheel_steps in range(21):
        control = bounds('sessions-deletion-dialog')
        dialog = bounds('settings-dialog')
        workspace = bounds('workspace')
        left, top = max(dialog.x, workspace.x), max(dialog.y, workspace.y)
        right = min(dialog.x + dialog.width, workspace.x + workspace.width)
        bottom = min(dialog.y + dialog.height, workspace.y + workspace.height)
        if (control.width > 0 and control.height > 0 and left <= control.x
                and top <= control.y and control.x + control.width <= right
                and control.y + control.height <= bottom):
            client.click('sessions-deletion-dialog')
            return
        if (wheel_steps == 20 or right <= left or bottom <= top or control.width <= 0
                or control.height <= 0 or control.x < left or control.x + control.width > right):
            raise RuntimeError('deletion control did not become fully visible')
        # AT-SPI rectangles and screen points are physical pixels at every scale.
        # Keep x over the Sessions controls, away from the navigation scroller.
        screen_x, screen_y = client._screen_point(client._find('settings-dialog'))
        x = screen_x + control.x + control.width // 2 - dialog.x - dialog.width // 2
        y = screen_y + (top + bottom) // 2 - dialog.y - dialog.height // 2
        command(client.env, 'xdotool', 'mousemove', str(x), str(y))
        command(client.env, 'xdotool', 'click', '5' if control.y + control.height > bottom else '4')
        wait_for('scroll did not move the deletion control',
                 lambda: bounds('sessions-deletion-dialog').y != control.y, timeout=3)


@contextmanager
def failure_evidence(client, output, report):
    """Capture a failed check while its client is still registered for cleanup."""
    try:
        yield
    except Exception as error:
        report.setdefault('error', str(error))
        # On relaunch, only the newest client should capture the failure.
        if 'failureEvidence' not in report:
            evidence = {'controls': [], 'captureErrors': []}
            report['failureEvidence'] = evidence
            try:
                evidence['process'] = (None if client.process is None else {
                    'pid': client.process.pid, 'exitCode': client.process.poll()})
            except Exception as capture_error:  # Evidence must not replace the failed check.
                evidence['captureErrors'].append({'stage': 'process', 'error': str(capture_error)})
            try:
                api = _atspi()
                targets = {'settings-dialog', 'settings-current-page', 'settings-search',
                           'sessions.permission-default-ask', 'sessions.permission-default-auto',
                           'sessions.permission-default-safety', 'sessions-deletion-dialog',
                           'sessions-work-enabled', 'sessions-show-chats'}
                for node in client._walk():
                    node.clear_cache()
                    states = node.get_state_set()
                    focused = states.contains(api.StateType.FOCUSED)
                    if node.get_accessible_id() not in targets and not focused:
                        continue
                    box = node.get_extents(api.CoordType.WINDOW)
                    evidence['controls'].append({
                        'id': node.get_accessible_id(), 'name': node.get_name(),
                        'role': node.get_role_name(), 'bounds': [box.x, box.y, box.width, box.height],
                        'focused': focused, 'showing': states.contains(api.StateType.SHOWING),
                        'checked': states.contains(api.StateType.CHECKED)})
                evidence['inputSubstitutions'] = list(client.substitutions)
            except Exception as capture_error:
                evidence['captureErrors'].append({'stage': 'controls', 'error': str(capture_error)})
            try:
                client.screenshot(output / 'failed.png')
            except Exception as capture_error:
                evidence['captureErrors'].append({'stage': 'screenshot', 'error': str(capture_error)})
        raise


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'checks': [],
              'limitations': ['Native-only focused outcomes; no matched-client parity claim.']}
    previous = accessibility_enabled()
    clients = []
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixture = cleanup.enter_context(HostFixture(
                Path('/tmp/opencode') / f'settings-pages-{uuid.uuid4().hex}', args.output / 'server', args, interrupts))
            fixture.create_session('stream-1200cps')
            put(fixture, INITIAL)
            proxy = SettingsFaults(fixture.url)
            cleanup.callback(proxy.stop)
            state = args.output / 'native-state'
            env = runner.native_env(state, proxy.url, 'openchamber-dark')
            for folder in runner.state_dirs(state).values():
                Path(folder).mkdir(parents=True, exist_ok=True)

            def launch(name):
                client_cleanup = cleanup.enter_context(ExitStack())
                log = client_cleanup.enter_context((args.output / f'native-{name}.log').open('w'))
                virtual_display.verify_owned_display()
                client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                client_cleanup.callback(client.close)
                clients.append(client)
                client_cleanup.enter_context(failure_evidence(client, args.output, report))
                client.launch()
                client.wait_ready(45, connected=True)
                session = object.__new__(Session)
                session.client, session.env, session.project, session.args = client, env, fixture.project, args
                return client, session, client_cleanup

            client, session, client_cleanup = launch('first')

            def record(name):
                client.screenshot(args.output / f'{name}.png')
                report['checks'].append(name)

            def saved(key):
                return fixture.get(DESKTOP).get(key)

            def observed(key, value):
                until(f'{key} did not persist as {value!r}', lambda: saved(key) == value)

            def open_settings():
                session.wait_id('settings')
                client.activate('settings')
                session.wait_id('settings-dialog')

            def page_is(title):
                session.wait_label('settings-current-page', lambda label: label == title, f'{title} did not open')

            open_settings()
            client.click('settings-nav-chat')
            page_is('Chat')
            session.wait_id('chat-prompt-navigator')
            assert session.has_state('chat-prompt-navigator', 'CHECKED')
            client.click('chat-prompt-navigator')
            observed('promptNavigatorEnabled', False)
            until('prompt navigator did not show its saved choice',
                  lambda: not session.has_state('chat-prompt-navigator', 'CHECKED'))
            record('chat-messages-prompt-navigator-saved')

            client.click('chat-settings-tab-composer')
            session.wait_id('chat.follow-up-behavior-steer')
            client.click('chat.follow-up-behavior-steer')
            observed('followUpBehavior', 'steer')
            until('Steer did not show as chosen', lambda: session.has_state('chat.follow-up-behavior-steer', 'CHECKED')
                  or session.has_state('chat.follow-up-behavior-steer', 'PRESSED'))
            record('chat-composer-follow-up-saved')

            # Search: a synonym opens Sessions and focuses the selected chip.
            client.click('settings-search')
            session.type('safety net')
            session.wait_id('settings-result-sessions-permission-default')
            session.keys('Return')
            page_is('Sessions')
            until('search did not focus the selected permission chip',
                  lambda: session.has_state('sessions.permission-default-ask', 'FOCUSED'))
            record('search-opens-sessions-permission-default')

            # Without a classification provider the safety net is not offered.
            client.click('sessions.permission-default-safety')
            client.click('sessions.permission-default-auto')
            observed('permissionDefaultMode', 'auto')
            assert proxy.settings_requests and not any(
                request['body'].get('permissionDefaultMode') == 'safety'
                for request in proxy.settings_requests if request['method'] == 'PUT'), 'disabled chip saved'
            record('disabled-safety-chip-saves-nothing')

            # A failed save keeps the choice on screen and offers Retry.
            proxy.fail_field = 'sessionWorkEnabled'
            client.click('sessions-work-enabled')
            session.wait_id('sessions-retry')
            session.wait_label('sessions-save-status', lambda label: label == 'Save failed', 'no failure status')
            assert not session.has_state('sessions-work-enabled', 'CHECKED'), 'failed choice was reverted'
            assert saved('sessionWorkEnabled') is True
            client.click('sessions-show-chats')
            observed('sidebarShowChatsSection', False)
            until('Chats section choice did not show', lambda: not session.has_state('sessions-show-chats', 'CHECKED'))
            assert not session.has_state('sessions-work-enabled', 'CHECKED'), 'failed choice was reverted'
            record('failed-save-retains-choice-independent-field-saves')
            client.activate('sessions-retry')
            observed('sessionWorkEnabled', False)
            session.wait_gone('sessions-retry')
            record('retry-persists')
            click_visible_deletion(client)
            observed('showDeletionDialog', False)
            session.keys('Escape')
            session.wait_gone('settings-dialog')

            # Restored: a new process shows what the server saved.
            client_cleanup.close()
            client, session, client_cleanup = launch('relaunch')
            open_settings()
            client.click('settings-nav-sessions')
            page_is('Sessions')
            session.wait_id('sessions-work-enabled')
            until('saved choices were not restored', lambda: (
                not session.has_state('sessions-work-enabled', 'CHECKED')
                and not session.has_state('sessions-show-chats', 'CHECKED')
                and not session.has_state('sessions-deletion-dialog', 'CHECKED')))
            client.click('settings-nav-chat')
            page_is('Chat')
            session.wait_id('chat-prompt-navigator')
            assert not session.has_state('chat-prompt-navigator', 'CHECKED')
            record('relaunch-restores-saved-choices')
            report['inputSubstitutions'] = client.substitutions
        report['passed'] = True
    except Exception as error:
        traceback.print_exc()
        report.setdefault('error', str(error))
        report['settingsRequests'] = proxy.settings_requests if 'proxy' in locals() else None
    finally:
        set_accessibility(previous)
        report['nativeProcesses'] = [{'pid': client.process.pid, 'exitCode': client.process.poll()}
                                     for client in clients if client.process]
        if binary_info(args.binary) != identity or any(item['exitCode'] is None for item in report['nativeProcesses']):
            report['passed'] = False
            report['cleanupError'] = 'binary changed or a native process survived cleanup'
            report.setdefault('error', report['cleanupError'])
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
