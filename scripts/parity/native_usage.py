#!/usr/bin/env python3
"""The Usage Settings page on an owned server and an owned Xvfb.

The isolated server has no provider credentials, so every provider answers
"not configured": the list, one read per provider, the selected provider's
not-configured state, the display switch and a refresh.
Native-only runtime evidence, not matched-client parity.
"""

import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import sys
import traceback
import urllib.parse
import uuid

import runner
import virtual_display
from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_files import Session
from native_general_settings import SettingsFaults
from native_hosts import HostFixture
from native_interactions import Interrupts, until
from native_platform import binary_info

PROVIDERS = 26


class QuotaRecorder(SettingsFaults):
    """Forwards everything and records the quota reads."""

    def __init__(self, upstream):
        self.quota_paths = []
        super().__init__(upstream)

    def handle(self, handler):
        path = urllib.parse.urlsplit(handler.path).path
        if path.startswith('/api/quota/'):
            with self.lock:
                self.quota_paths.append(path)
        super().handle(handler)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'checks': []}
    previous = accessibility_enabled()
    clients = []
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixture = cleanup.enter_context(HostFixture(
                Path('/tmp/opencode') / f'usage-{uuid.uuid4().hex}', args.output / 'server', args, interrupts))
            proxy = QuotaRecorder(fixture.url)
            cleanup.callback(proxy.stop)
            state = args.output / 'native-state'
            env = runner.native_env(state, proxy.url, 'openchamber-dark')
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

            def quota_reads():
                with proxy.lock:
                    return list(proxy.quota_paths)

            session.wait_id('settings')
            client.activate('settings')
            session.wait_id('settings-dialog')
            client.click('settings-nav-usage')
            session.wait_id('usage-provider-claude')
            until('not every provider was read once', lambda: len(quota_reads()) == PROVIDERS, 30)
            assert len(set(quota_reads())) == PROVIDERS, 'a provider was read twice'
            client.click('usage-provider-codex')
            session.wait_label('usage-provider-title', lambda label: label == 'Codex usage', 'Codex did not open')
            session.wait_id('usage-not-configured')
            client.screenshot(args.output / 'usage-not-configured.png')
            report['checks'].append('every-provider-read-and-not-configured-shown')
            client.click('usage-display-remaining')
            until('the display switch did not take', lambda: session.has_state('usage-display-remaining', 'CHECKED')
                  or session.has_state('usage-display-remaining', 'PRESSED'), 10)
            client.click('usage-refresh')
            until('refresh did not read every provider again', lambda: len(quota_reads()) == 2 * PROVIDERS, 30)
            report['checks'].append('refresh-reads-again')
            report['quotaReads'] = len(quota_reads())
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
