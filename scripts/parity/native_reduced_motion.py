#!/usr/bin/env python3
"""Native follows the desktop's reduced-motion preference, on an owned Xvfb.

A stub XDG desktop portal on the run's private session bus answers
`org.freedesktop.appearance reduced-motion` and records every read. Native
must ask for it at startup, whichever value the desktop holds. What the flag
gates inside GPUI Kit (dialog entry, transitions, presence) is covered by the
kit's own tests; a screenshot cannot tell an undrawn frame from an animating one.
"""

import argparse
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import traceback

import runner
import virtual_display
from native_adapter import NativeClient, accessibility_enabled, set_accessibility
from native_interactions import until
from native_platform import binary_info, wait_for

PORTAL = 'org.freedesktop.portal.Desktop'
PORTAL_XML = """
<node>
  <interface name="org.freedesktop.portal.Settings">
    <method name="ReadAll"><arg type="as" direction="in"/><arg type="a{sa{sv}}" direction="out"/></method>
    <method name="Read"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
    <method name="ReadOne"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
    <signal name="SettingChanged"><arg type="s"/><arg type="s"/><arg type="v"/></signal>
    <property name="version" type="u" access="read"/>
  </interface>
</node>
"""


class Portal:
    """Owns the portal name on the private bus and records reduced-motion reads."""

    def __init__(self):
        import gi
        gi.require_version('Gio', '2.0')
        from gi.repository import Gio, GLib
        self.Gio, self.GLib = Gio, GLib
        self.reduced = False
        self.reads = []
        self.owned = threading.Event()
        self.loop = GLib.MainLoop()
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        if not self.owned.wait(10):
            raise RuntimeError('the stub portal could not own its name')

    def _serve(self):
        Gio, GLib = self.Gio, self.GLib
        connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        info = Gio.DBusNodeInfo.new_for_xml(PORTAL_XML).interfaces[0]
        def call(_connection, _sender, _path, _interface, method, parameters, invocation):
            value = GLib.Variant('u', 1 if self.reduced else 0)
            if method == 'ReadAll':
                invocation.return_value(GLib.Variant('(a{sa{sv}})', ({
                    'org.freedesktop.appearance': {'reduced-motion': value}},)))
                return
            namespace, key = parameters.unpack()
            self.reads.append({'method': method, 'namespace': namespace, 'key': key})
            if (namespace, key) != ('org.freedesktop.appearance', 'reduced-motion'):
                invocation.return_dbus_error('org.freedesktop.portal.Error.NotFound', 'not stubbed')
            elif method == 'Read':
                invocation.return_value(GLib.Variant('(v)', (GLib.Variant('v', value),)))
            else:
                invocation.return_value(GLib.Variant('(v)', (value,)))

        def get(*_):
            return GLib.Variant('u', 2)

        connection.register_object('/org/freedesktop/portal/desktop', info, call, get, None)
        Gio.bus_own_name_on_connection(connection, PORTAL, Gio.BusNameOwnerFlags.NONE,
                                       lambda *_: self.owned.set(), None)
        self.loop.run()

    def stop(self):
        self.loop.quit()


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'runs': {}}
    previous = accessibility_enabled()
    try:
        set_accessibility(True)
        portal = Portal()
        try:
            for reduced in (True, False):
                name = 'reduced' if reduced else 'animated'
                portal.reduced = reduced
                portal.reads.clear()
                state = Path(tempfile.mkdtemp(prefix=f'{name}-', dir=args.output))
                env = runner.native_env(state, None, 'openchamber-dark')
                env.pop('OPENCHAMBER_NATIVE_SERVER', None)
                for folder in runner.state_dirs(state).values():
                    Path(folder).mkdir(parents=True, exist_ok=True)
                with (args.output / f'native-{name}.log').open('w') as log:
                    client = NativeClient(args.binary, env, args.scale, 1280, 800, stderr=log)
                    try:
                        client.launch()
                        client.wait_ready(45, connected=False)
                        until('native did not read the reduced-motion preference', lambda: any(
                            read['key'] == 'reduced-motion' for read in portal.reads), 10)
                        client.activate('settings')
                        wait_for('Settings did not open', lambda: client._find('settings-dialog'), 10)
                        time.sleep(1)
                        client.screenshot(args.output / f'{name}-settings.png')
                    finally:
                        client.close()
                report['runs'][name] = {'portalReads': list(portal.reads)}
        finally:
            portal.stop()
        report['passed'] = True
    except Exception as error:
        traceback.print_exc()
        report['error'] = str(error)
    finally:
        set_accessibility(previous)
        if binary_info(args.binary) != identity:
            report.update(passed=False, error='binary changed during the run')
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--scale', type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    args = parser.parse_args()
    for key in ('binary', 'output'):
        setattr(args, key, getattr(args, key).resolve())
    if args.output.parent != Path('/tmp/opencode') or args.output.exists():
        parser.error('--output must be new and directly under /tmp/opencode')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1280 * args.scale), round(800 * args.scale))
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
