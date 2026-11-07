#!/usr/bin/env python3
"""Observe native public-asset egress through a rejecting loopback proxy, the
enterprise-gated integrations button, and the recovery screen for a server
that was never reached."""

import argparse
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sys
import threading
import uuid

import runner
import virtual_display
from native_adapter import NativeClient
from native_hosts import HostFixture
from native_interactions import Interrupts, accessibility_enabled, set_accessibility, until
from native_platform import binary_info


class AssetProxy:
    def __init__(self):
        self.requests = []
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_CONNECT(self):
                with owner.lock:
                    owner.requests.append(self.path)
                self.send_error(502, 'Synthetic proxy refuses outbound connections')

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def count(self):
        with self.lock:
            return len(self.requests)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def present(client, accessible_id):
    return any((node.get_accessible_id() or '') == accessible_id for node in client._walk())


def closed_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def check_recovery(args, cleanup, report):
    """A closed port shows the recovery screen with Retry and Use Remote, and no workspace."""
    url = f'http://127.0.0.1:{closed_port()}/'
    env = runner.native_env(args.output / 'unreachable', url, 'openchamber-dark')
    env['OPENCHAMBER_ENTERPRISE_MODE'] = '0'
    log = cleanup.enter_context((args.output / 'unreachable.log').open('w'))
    client = NativeClient(args.binary, env, 1.25, 1280, 800, stderr=log)
    cleanup.callback(client.close)
    client.launch()
    client.wait_ready(45, connected=False)
    until('the recovery screen did not appear', lambda: present(client, 'connection-recovery'), 30)
    for required in ('connection-recovery-address', 'connection-recovery-retry', 'connection-recovery-use-remote'):
        if not present(client, required):
            raise RuntimeError(f'the recovery screen lacks {required}')
    if present(client, 'composer'):
        raise RuntimeError('the workspace is drawn behind the recovery screen')
    client.screenshot(args.output / 'unreachable.png')
    client.activate('connection-recovery-retry')
    until('the recovery screen closed after Retry', lambda: present(client, 'connection-recovery'), 10)
    report['checks'].append({'mode': 'unreachable', 'recoveryScreen': True})
    client.close()


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'checks': []}
    previous = accessibility_enabled()
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixture = cleanup.enter_context(HostFixture(Path('/tmp/opencode') / f'policy-{uuid.uuid4().hex}', args.output / 'fixture', args, interrupts))
            proxy = AssetProxy()
            cleanup.callback(proxy.close)
            for restricted in ([True] if args.expect_assets else [True, False]):
                name = 'restricted' if restricted else 'normal'
                env = runner.native_env(args.output / name, fixture.url, 'openchamber-dark')
                address = f'http://127.0.0.1:{proxy.server.server_port}'
                env.update(HTTPS_PROXY=address, https_proxy=address, NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost',
                           OPENCHAMBER_ENTERPRISE_MODE='1' if restricted else '0')
                log = cleanup.enter_context((args.output / f'{name}.log').open('w'))
                client = NativeClient(args.binary, env, 1.25, 1280, 800, stderr=log)
                cleanup.callback(client.close)
                before = proxy.count()
                client.launch()
                client.wait_ready(45, connected=True)
                until('the sidebar did not settle on the policy',
                      lambda: present(client, 'integrations') != restricted, 20)
                if args.expect_assets or not restricted:
                    until('normal public asset request did not reach the proxy', lambda: proxy.count() > before, 30)
                else:
                    client.wait_for_marker('policy-checked', 20)
                    client.wait_for_marker('public-asset-blocked', 20)
                    if proxy.count() != before:
                        raise RuntimeError('enterprise mode sent a public asset request')
                report['checks'].append({'mode': name, 'outboundAttempts': proxy.count() - before,
                                         'integrationsShown': not restricted})
                client.close()
            check_recovery(args, cleanup, report)
        report['passed'] = True
    except Exception as error:
        report['error'] = str(error)
    finally:
        set_accessibility(previous)
        if binary_info(args.binary) != identity:
            report.update(passed=False, error='candidate changed')
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--dist', type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument('--opencode-binary', type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument('--expect-assets', action='store_true', help='record the pre-fix public request while enterprise mode is on')
    args = parser.parse_args()
    for key in ('binary', 'output', 'dist', 'opencode_binary'):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path('/tmp/opencode') or args.output.exists():
        parser.error('--output must be a new directory directly under /tmp/opencode')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], 1600, 1000)
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
