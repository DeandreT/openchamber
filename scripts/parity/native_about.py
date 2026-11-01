#!/usr/bin/env python3
"""Read native About versions against a private real server and retain a draft."""

import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import sys
import uuid
import urllib.parse
import tomllib

import runner
import virtual_display
from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient
from native_hosts import HostFixture
from native_interactions import Interrupts, Interaction, accessibility_enabled, set_accessibility, until
from native_platform import binary_info, command, editable_text
from workflow_proxy import FaultProxy


class VersionsProxy(FaultProxy):
    def __init__(self, upstream):
        self.fail_app = False
        super().__init__(upstream, 'before')

    def handle(self, handler):
        if self.fail_app and urllib.parse.urlsplit(handler.path).path == '/api/system/info':
            self._reply(handler, 503, [('Content-Type', 'application/json')], b'{}')
            return
        super().handle(handler)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    info = binary_info(args.binary)
    report = {'passed': False, 'binary': info, 'display': 'owned-xvfb'}
    previous = accessibility_enabled()
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixture = cleanup.enter_context(HostFixture(Path('/tmp/opencode') / f'about-{uuid.uuid4().hex}', args.output / 'fixture', args, interrupts))
            expected_app = fixture.get('/api/system/info')['openchamberVersion']
            expected_engine = fixture.get('/api/opencode/upgrade-status')['currentVersion']
            proxy = VersionsProxy(fixture.url)
            cleanup.callback(proxy.stop)
            env = runner.native_env(args.output / 'state', proxy.url, 'openchamber-light')
            opener_dir = args.output / 'url-opener'
            opener_dir.mkdir(mode=0o700)
            opened = args.output / 'opened-urls.jsonl'
            opener = opener_dir / 'xdg-open'
            opener.write_text('#!/usr/bin/python3\nimport json, sys\nfrom pathlib import Path\n'
                              f'with Path({str(opened)!r}).open("a") as output:\n    output.write(json.dumps(sys.argv[1:]) + "\\n")\n')
            opener.chmod(0o700)
            env['PATH'] = str(opener_dir) + ':' + env['PATH']
            env['BROWSER'] = str(opener)
            log = cleanup.enter_context((args.output / 'native.log').open('w'))
            client = NativeClient(args.binary, env, 1.25, 1280, 800, stderr=log)
            cleanup.callback(client.close)
            def capture_last():
                if client.process is not None and client.process.poll() is None:
                    client.screenshot(args.output / 'last-state.png')
            cleanup.callback(capture_last)
            client.launch()
            client.wait_ready(45, connected=True)
            probe = object.__new__(Interaction)
            probe.client, probe.env, probe.clipboard = client, env, ClipboardProbe()
            composer = until('composer unavailable', probe.composer, 20)
            draft = 'Keep this draft while About is open.'
            probe.paste(composer, draft)
            until('draft not entered', lambda: editable_text(probe.composer()) == draft, 10)
            client.click('about')
            if not client.wait_for({'elements': ['about-dialog', 'about-app-version', 'about-engine-version']}, 15):
                raise RuntimeError('About did not open')
            def app_version_matches():
                label = client._find('about-app-version').get_name()
                report['appVersionLabel'] = label
                return label == f'OpenChamber version {expected_app}'
            until('wrong OpenChamber version', app_version_matches, 30)
            until('wrong OpenCode version', lambda: client._find('about-engine-version').get_name() == f'OpenCode version {expected_engine}', 30)
            for key in ('about-github', 'about-discord', 'about-social'):
                if client._find(key).get_role_name() != 'link':
                    raise RuntimeError('About links do not expose link semantics')
            def focused(key):
                from native_adapter import _atspi
                node = client._find(key)
                node.clear_cache()
                return node.get_state_set().contains(_atspi().StateType.FOCUSED)
            for _ in range(8):
                command(env, 'xdotool', 'key', '--clearmodifiers', 'Tab')
                client._read_marks(0.1)
                if focused('about-github'):
                    break
            if not focused('about-github'):
                raise RuntimeError('Tab did not reach the About links')
            expected_urls = []
            for index, (key, url) in enumerate([
                ('about-github', 'https://github.com/openchamber/openchamber'),
                ('about-discord', 'https://discord.gg/ZYRSdnwwKA'),
                ('about-social', 'https://x.com/openchamber_dev'),
            ]):
                until('Tab skipped an About link', lambda: focused(key), 10)
                command(env, 'xdotool', 'key', '--clearmodifiers', 'Return')
                until('Enter did not dispatch the link', lambda: opened.is_file() and len(opened.read_text().splitlines()) >= index + 1, 10)
                urls = [json.loads(line) for line in opened.read_text().splitlines()]
                expected_urls.append([url])
                if urls != expected_urls:
                    raise RuntimeError('About dispatched an incorrect or duplicate URL')
                if index < 2:
                    command(env, 'xdotool', 'key', '--clearmodifiers', 'Tab')
            client.screenshot(args.output / 'about.png')
            command(env, 'xdotool', 'key', '--clearmodifiers', 'Escape')
            until('About did not close', lambda: 'about-dialog' not in {node.get_accessible_id() for node in client._walk()}, 10)
            if editable_text(probe.composer()) != draft:
                raise RuntimeError('About changed the composer draft')
            proxy.fail_app = True
            client.activate('about')
            if not client.wait_for({'elements': ['about-dialog']}, 15):
                raise RuntimeError('About accessibility activation failed')
            native_version = tomllib.loads((runner.REPO_ROOT / 'native/Cargo.toml').read_text())['workspace']['package']['version']
            until('failed app-version lookup did not use the native fallback', lambda: client._find('about-app-version').get_name() == f'OpenChamber version {native_version}', 30)
            until('failed app-version lookup lost the independent engine version', lambda: client._find('about-engine-version').get_name() == f'OpenCode version {expected_engine}', 30)
            if client.substitutions:
                raise RuntimeError('pointer activation used an accessibility substitute')
        report.update(passed=True, externalBrowser='private recording stub', checks=['pointer-open', 'server-versions', 'keyboard-links', 'escape-close', 'draft-retained', 'accessible-open', 'independent-version-failure'])
    except Exception as error:
        report['error'] = str(error)
    finally:
        set_accessibility(previous)
        if binary_info(args.binary) != info:
            report.update(passed=False, error='candidate changed during qualification')
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--dist', type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument('--opencode-binary', type=Path, default=runner.DEFAULT_OPENCODE)
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
