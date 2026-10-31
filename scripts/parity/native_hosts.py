#!/usr/bin/env python3
"""Native direct-host profiles and runtime switching on a private display."""

import argparse
from contextlib import ExitStack
import json
import stat
from pathlib import Path
import sys
import uuid

import runner
import virtual_display
from native_adapter import NativeClient, _atspi
from native_interactions import Fixture, Interaction, Interrupts, accessibility_enabled, set_accessibility, until
from native_platform import binary_info, command, editable_text
from clipboard_probe import ClipboardProbe


class HostFixture(Fixture):
    MODE = "--selections"


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    result = {"passed": False, "binary": identity, "display": "owned-xvfb"}
    previous = accessibility_enabled()
    clients = []
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            first = cleanup.enter_context(HostFixture(Path('/tmp/opencode') / f'host-alpha-{uuid.uuid4().hex}', args.output / 'alpha', args, interrupts))
            second = cleanup.enter_context(HostFixture(Path('/tmp/opencode') / f'host-beta-{uuid.uuid4().hex}', args.output / 'beta', args, interrupts))
            sessions = [fixture.create_session('stream-1200cps') for fixture in (first, second)]
            state = args.output / 'native-state'
            env = runner.native_env(state, first.url, 'openchamber-dark')
            env.pop('OPENCHAMBER_NATIVE_SERVER', None)
            env.pop('OPENCHAMBER_NATIVE_PASSWORD', None)
            def launch(name):
                log = cleanup.enter_context((args.output / f'native-{name}.log').open('w'))
                client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                cleanup.callback(client.close)
                clients.append(client)
                def capture_last():
                    if client.process is not None and client.process.poll() is None:
                        try:
                            client.screenshot(args.output / f'{name}-last-state.png')
                        except Exception:
                            pass
                cleanup.callback(capture_last)
                client.launch()
                client.wait_ready(40, connected=False)
                return client

            client = launch('first')
            profile_path = state / 'config/openchamber-native/hosts.json'

            def profiles():
                return json.loads(profile_path.read_text()) if profile_path.is_file() else {'hosts': []}

            def click(key):
                if not client.wait_for({'elements': [key]}, 15):
                    raise RuntimeError(f'host control did not appear: {key}')
                client.click(key)

            def fill(key, text):
                click(key)
                command(env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a')
                command(env, 'xdotool', 'type', '--clearmodifiers', text)

            def add(label, url):
                click('host-add')
                fill('host-label', label)
                fill('host-url', url)
                click('host-save')
                return until('host was not saved', lambda: next((host for host in profiles()['hosts'] if host['label'] == label), None), 15)

            def select_session(index):
                sid = sessions[index]['id']
                def choose():
                    rows = [row for row in client.rows() if row['key'].endswith(':session:' + sid)]
                    if len(rows) != 1:
                        return False
                    try:
                        client.click(rows[0]['key'])
                        return True
                    except LookupError:
                        # Topology can move the same session from Chats to its
                        # project between the row read and the pointer action.
                        return False
                until('selected server sessions did not arrive', choose, 45)
                until('host selected another session', lambda: any(
                    row['selected'] and row['key'].endswith(':session:' + sid) for row in client.rows()), 20)
                other_ids = {session['id'] for session in sessions if session['id'] != sid}
                until('sessions from the previous host remained visible', lambda: not any(
                    row['key'].rsplit(':session:', 1)[-1] in other_ids for row in client.rows()), 20)

            click('instance-switcher')
            alpha = add('Alpha', first.url)
            beta = add('Beta', second.url)
            click('host-add')
            fill('host-label', 'Duplicate')
            fill('host-url', first.url)
            click('host-save')
            until('duplicate address was not reported', lambda: 'already saved' in (client._find('host-error').get_name() or ''), 10)
            if len(profiles()['hosts']) != 2:
                raise RuntimeError('duplicate address changed the saved profiles')
            fill('host-url', 'not-an-address')
            click('host-save')
            until('invalid address was not reported', lambda: 'Invalid URL' in (client._find('host-error').get_name() or ''), 10)
            click('host-cancel')
            click('host-edit:' + beta['id'])
            fill('host-label', 'Beta renamed')
            click('host-save')
            until('profile label was not edited', lambda: any(item['id'] == beta['id'] and item['label'] == 'Beta renamed' for item in profiles()['hosts']), 15)

            click('host-add')
            fill('host-label', 'Retained after save failure')
            fill('host-url', 'http://127.0.0.1:1/')
            profile_directory = profile_path.parent
            mode = stat.S_IMODE(profile_directory.stat().st_mode)
            try:
                profile_directory.chmod(0o500)
                click('host-save')
                until('failed save was not reported', lambda: 'Failed to save' in (client._find('host-error').get_name() or ''), 15)
                if editable_text(client._find('host-label')) != 'Retained after save failure':
                    raise RuntimeError('failed save discarded the profile draft')
                if len(profiles()['hosts']) != 2:
                    raise RuntimeError('failed save changed the saved profiles')
            finally:
                profile_directory.chmod(mode)
            click('host-save')
            until('profile save retry failed', lambda: len(profiles()['hosts']) == 3, 15)
            click('host-connect:' + alpha['id'])
            select_session(0)
            probe = object.__new__(Interaction)
            probe.client = client
            probe.env = env
            probe.clipboard = ClipboardProbe()
            entry = until('composer absent', probe.composer, 15)
            until('Alpha composer did not become editable', lambda: entry.get_state_set().contains(_atspi().StateType.EDITABLE), 20)
            draft = 'Draft belongs to Alpha.'
            probe.paste(entry, draft)
            until('Alpha draft was not entered', lambda: editable_text(entry) == draft, 10)
            click('instance-switcher')
            click('host-connect:' + beta['id'])
            select_session(1)
            until('Beta composer absent', probe.composer, 15)
            if editable_text(probe.composer()) == draft:
                raise RuntimeError('Alpha draft leaked to Beta')
            click('instance-switcher')
            alpha_default = 'host-default:' + alpha['id']
            beta_default = 'host-default:' + beta['id']
            if not client.wait_for({'elements': [alpha_default, beta_default]}, 15):
                raise RuntimeError('default controls did not appear')
            until('default action did not enable', lambda: client._find(alpha_default).get_state_set().contains(_atspi().StateType.ENABLED), 15)
            client.activate(alpha_default)
            until('accessible default action did not save', lambda: profiles().get('defaultHost') == alpha['id'], 15)
            until('keyboard default action did not enable', lambda: client._find(beta_default).get_state_set().contains(_atspi().StateType.ENABLED), 15)
            if not client._find(beta_default).get_component_iface().grab_focus():
                raise RuntimeError('default action could not receive keyboard focus')
            command(env, 'xdotool', 'key', '--clearmodifiers', 'space')
            until('default host was not saved', lambda: profiles().get('defaultHost') == beta['id'], 15)
            click('host-connect:' + alpha['id'])
            select_session(0)
            until('Alpha draft was not restored', lambda: editable_text(probe.composer()) == draft, 15)
            client.screenshot(args.output / 'alpha-restored.png')
            client.close()
            client = launch('default')
            select_session(1)
            client.screenshot(args.output / 'default-beta.png')
            result['checks'] = ['add', 'duplicate-address', 'invalid-address', 'edit', 'save-failure-retains-draft',
                                'save-retry', 'switch', 'draft-isolation', 'accessible-default', 'keyboard-default', 'default-restart']
            result['inputSubstitutions'] = [item for active in clients for item in active.substitutions]
            if result['inputSubstitutions']:
                raise RuntimeError('host input required an accessibility substitution')
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
    finally:
        try:
            set_accessibility(previous)
        except Exception:
            result.update(passed=False, error='private accessibility state could not be restored')
        if binary_info(args.binary) != identity:
            result.update(passed=False, error='binary changed during qualification')
        result['nativeProcesses'] = [{'pid': client.process.pid, 'exitCode': client.process.poll()}
                                     for client in clients if client.process is not None]
        if any(item['exitCode'] is None for item in result['nativeProcesses']):
            result.update(passed=False, error='a native client remained after cleanup')
        (args.output / 'report.json').write_text(json.dumps(result, indent=2) + '\n')
    return 0 if result['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--dist', type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument('--opencode-binary', type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument('--scale', type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    for key in ('binary', 'dist', 'opencode_binary', 'output'):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path('/tmp/opencode') or args.output.exists():
        parser.error('--output must be a new directory directly under /tmp/opencode')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
