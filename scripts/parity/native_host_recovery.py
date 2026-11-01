#!/usr/bin/env python3
"""Recover an invalid native host profile without replacing it or showing fixture sessions."""

import argparse
from contextlib import ExitStack
import json
import stat
from pathlib import Path
import sys
import uuid

import png
import runner
import virtual_display
from native_adapter import NativeClient, _atspi
from native_hosts import HostFixture
from native_interactions import Interrupts, accessibility_enabled, set_accessibility, until
from native_platform import binary_info


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb'}
    enabled = accessibility_enabled()
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixture = cleanup.enter_context(HostFixture(
                Path('/tmp/opencode') / f'host-recovery-{uuid.uuid4().hex}', args.output / 'fixture', args, interrupts))
            session = fixture.create_session('stream-1200cps')
            env = runner.native_env(args.output / 'state', fixture.url, 'openchamber-dark')
            env.pop('OPENCHAMBER_NATIVE_SERVER', None)
            env.pop('OPENCHAMBER_NATIVE_WORKLOAD', None)
            profile = Path(env['XDG_CONFIG_HOME']) / 'openchamber-native/hosts.json'
            profile.parent.mkdir(parents=True, exist_ok=True)
            broken = b'{unreadable profile fixture'
            profile.write_bytes(broken)
            log = cleanup.enter_context((args.output / 'native.log').open('w'))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(40, connected=False)
            if not client.wait_for({'elements': ['host-startup-recovery', 'host-startup-retry']}, 10):
                client.screenshot(args.output / 'missing-recovery.png')
                raise RuntimeError('invalid saved hosts did not show recovery')
            if any(node.get_accessible_id() in {'sidebar', 'composer'} for node in client._walk()):
                raise RuntimeError('failed profile load showed synthetic or previous sessions')
            if profile.read_bytes() != broken:
                raise RuntimeError('startup rewrote the invalid profile')
            def painted_recovery():
                image = args.output / 'recovery.png'
                client.screenshot(image)
                pixels = png.read(image).rgba
                colors = set()
                for offset in range(0, len(pixels), 4):
                    colors.add(bytes(pixels[offset:offset + 4]))
                    if len(colors) > 10:
                        return True
                return False
            until('recovery window stayed blank', painted_recovery, 15)
            mode = stat.S_IMODE(profile.stat().st_mode)
            try:
                # A distinct second error proves this attempt settled. The
                # original invalid-JSON error cannot satisfy the wait.
                profile.chmod(0)
                client.activate('host-startup-retry')
                def retry_failed():
                    control = client._find('host-startup-retry')
                    control.clear_cache()
                    return (control.get_state_set().contains(_atspi().StateType.ENABLED)
                            and client._find('host-startup-error').get_name() == 'Failed to load')
                until('failed retry did not settle', retry_failed, 15)
            finally:
                profile.chmod(mode)
            if profile.read_bytes() != broken:
                raise RuntimeError('retry rewrote the invalid profile')
            profile.write_text(json.dumps({'version': 1, 'revision': 1, 'hosts': [
                {'id': 'host_recovered', 'label': 'Recovered fixture', 'url': fixture.url}
            ], 'defaultHost': 'host_recovered'}))
            client.click('host-startup-retry')
            client.wait_for_marker('sessions-shown', 45)
            until('recovered host sessions did not appear', lambda: any(
                row['key'].endswith(':session:' + session['id']) for row in client.rows()), 20)
            from native_interactions import Interaction
            probe = object.__new__(Interaction)
            probe.client = client
            until('recovery did not restore composer keyboard focus', lambda: probe.composer() is not None
                  and probe.composer().get_state_set().contains(_atspi().StateType.FOCUSED), 15)
            if 'host-startup-recovery' in {node.get_accessible_id() for node in client._walk()}:
                raise RuntimeError('successful retry left recovery visible')
            client.screenshot(args.output / 'recovered.png')
            if client.substitutions:
                raise RuntimeError('pointer path used an accessibility substitute')

            client.close()
            profile.write_bytes(broken)
            second_log = cleanup.enter_context((args.output / 'native-no-default.log').open('w'))
            client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=second_log)
            cleanup.callback(client.close)
            client.launch()
            client.wait_ready(40, connected=False)
            if not client.wait_for({'elements': ['host-startup-retry']}, 10):
                raise RuntimeError('second recovery did not appear')
            profile.write_text(json.dumps({'version': 1, 'revision': 2, 'hosts': [
                {'id': 'host_recovered', 'label': 'Recovered fixture', 'url': fixture.url}
            ], 'defaultHost': None}))
            client.click('host-startup-retry')
            if not client.wait_for({'elements': ['host-connect:host_recovered']}, 15):
                raise RuntimeError('a valid file without a default did not offer the picker')
            client.click('host-connect:host_recovered')
            client.wait_for_marker('sessions-shown', 45)
            probe.client = client
            until('choosing a recovered host did not focus the composer', lambda: probe.composer() is not None
                  and probe.composer().get_state_set().contains(_atspi().StateType.FOCUSED), 15)
            if client.substitutions:
                raise RuntimeError('picker input used an accessibility substitute')
            report['checks'] = ['invalid-profile', 'no-synthetic-sessions', 'unreadable-retry',
                                'preserved-file', 'recovered-default', 'no-default-picker', 'composer-focus']
        report['passed'] = True
    except Exception as error:
        report['error'] = str(error)
    finally:
        set_accessibility(enabled)
        if binary_info(args.binary) != identity:
            report.update(passed=False, error='candidate changed during qualification')
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--dist', type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument('--opencode-binary', type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument('--scale', type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    args = parser.parse_args()
    for key in ('binary', 'output', 'dist', 'opencode_binary'):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path('/tmp/opencode') or args.output.exists():
        parser.error('--output must be a new directory directly under /tmp/opencode')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
