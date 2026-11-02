#!/usr/bin/env python3
"""Measure live Appearance changes in an existing native terminal on private Xvfb.

Accepts an already-built, frozen binary. Creates one owned server and one PTY,
drives Settings and the terminal with real pointer/key input, and measures only
the painted terminal interior after Settings closes. This is native-only proof.
"""

import argparse
from collections import Counter
from contextlib import ExitStack
import json
import math
import os
from pathlib import Path
import shlex
import sys
import time
import traceback
import urllib.parse
import uuid

from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient
from native_appearance import AppearanceInteraction, native_environment, validate_output
from native_hosts import HostFixture
from native_interactions import Interrupts, accessibility_enabled, process_identity, set_accessibility, until
from native_platform import binary_info, command
from native_terminal import TerminalProxy
import png
import runner
import virtual_display


MARKER = 'NATIVETERMINALAPPEARANCE673915'
PAINT = ('\x1b[?25l\x1b[0m\x1b[2J\x1b[H' + MARKER + '\r\n'
         + 'Il1 O0 Qq ag fi fl 0123456789 []{}()<>\r\n'
         + 'MMMM WWWW mmmm wwww @#$% &* += /\\\r\n'
         + 'Glyph paint stays still while Settings changes.\r\n'
         + '0123456789 abcdefghijklmnopqrstuvwxyz ABCDEFGHIJ\r\n'
         + 'Il1 O0 Qq ag fi fl 0123456789 []{}()<>\r\n')

# Runs through the real shell. It disables echo and the cursor, writes a fixed
# grid once, then records numbered key probes without changing terminal output.
# The parent verifies this process's PID/starttime and exact PTY input on every
# checkpoint. A theme/font change must not replace it or reconnect the socket.
PTY_PROGRAM = r'''
import json, os, pathlib, sys, termios
root = pathlib.Path(sys.argv[1])
paint = (root / 'paint.bin').read_bytes()
old = termios.tcgetattr(0)
attrs = termios.tcgetattr(0)
attrs[1] &= ~termios.OPOST
attrs[3] &= ~(termios.ECHO | termios.ICANON)
attrs[6][termios.VMIN] = 1
attrs[6][termios.VTIME] = 0
termios.tcsetattr(0, termios.TCSANOW, attrs)
try:
    pending = paint
    while pending:
        pending = pending[os.write(1, pending):]
    info = {'pid': os.getpid(), 'parentPid': os.getppid(),
            'stdin': os.ttyname(0), 'stdout': os.ttyname(1)}
    (root / 'process.json').write_text(json.dumps(info))
    (root / 'written.bin').write_bytes(paint)
    with (root / 'input.bin').open('wb', buffering=0) as capture:
        while True:
            data = os.read(0, 4096)
            if not data:
                break
            capture.write(data)
finally:
    termios.tcsetattr(0, termios.TCSANOW, old)
'''


def grid_crop(rect, scale, image_size, *, glyphs=False):
    """Physical crop strictly inside the grid, excluding its edges/scrollbar.

    Glyph evidence also excludes the selected marker's first row. Terminal
    padding is 4 logical pixels; the inset leaves neither panel chrome nor
    the outer view's freshly rendered palette in the measured image.
    """
    if len(rect) != 4 or not all(math.isfinite(value) for value in (*rect, scale)) or scale <= 0:
        raise ValueError('invalid terminal geometry or scale')
    x, y, width, height = rect
    left, top = math.ceil((x + 12) * scale), math.ceil((y + (48 if glyphs else 12)) * scale)
    right, bottom = math.floor((x + width - 24) * scale), math.floor((y + height - 24) * scale)
    if (x < 0 or y < 0 or width < 180 or height < 140 or left < 0 or top < 0
            or (x + width) * scale > image_size[0] + 1 or (y + height) * scale > image_size[1] + 1
            or right > image_size[0] or bottom > image_size[1] or right <= left or bottom <= top):
        raise ValueError(f'terminal grid has no safe interior crop: {rect}, scale={scale}, image={image_size}')
    return [left, top, right - left, bottom - top]


def paint_metrics(image):
    colors = Counter(tuple(image.rgba[index:index + 3]) for index in range(0, len(image.rgba), 4))
    if not colors:
        raise ValueError('empty terminal crop')
    background, count = colors.most_common(1)[0]
    ink = []
    rows = {}
    for y in range(image.height):
        for x in range(image.width):
            pixel = image.pixel(x, y)[:3]
            if max(abs(a - b) for a, b in zip(pixel, background)) > 12:
                ink.append((x, y))
                row = rows.setdefault(y, [x, x, 0])
                row[0], row[1], row[2] = min(row[0], x), max(row[1], x), row[2] + 1
    return {'background': list(background), 'backgroundShare': count / (image.width * image.height),
            'luminance': sum(channel * weight for channel, weight in zip(background, (0.2126, 0.7152, 0.0722))),
            'inkPixels': len(ink),
            'inkBounds': [min(x for x, _ in ink), min(y for _, y in ink),
                          max(x for x, _ in ink), max(y for _, y in ink)] if ink else None,
            'inkRows': [[y, *row] for y, row in sorted(rows.items())]}


def compare_paint(before, after, diff_path):
    if (before.width, before.height) != (after.width, after.height):
        raise RuntimeError('the measured terminal crop changed size')
    first, second = paint_metrics(before), paint_metrics(after)
    changed = ink_changed = ink_union = 0
    difference = bytearray()
    for index in range(0, len(before.rgba), 4):
        old, new = before.rgba[index:index + 3], after.rgba[index:index + 3]
        differs = max(abs(a - b) for a, b in zip(old, new)) > 3
        ink = (max(abs(a - b) for a, b in zip(old, first['background'])) > 12
               or max(abs(a - b) for a, b in zip(new, second['background'])) > 12)
        changed += differs
        ink_union += ink
        ink_changed += differs and ink
        difference.extend((255, 40, 80, 255) if differs else (24, 24, 24, 255))
    png.write(diff_path, png.Image(before.width, before.height, difference))
    return {'before': first, 'after': second, 'changedPixels': changed,
            'inkChangedPixels': ink_changed, 'inkUnionPixels': ink_union,
            'diff': str(diff_path)}


def verify_theme(dark, light, restored):
    for name, metrics in (('dark', dark), ('light', light), ('restored', restored)):
        if metrics['backgroundShare'] < 0.65 or metrics['inkPixels'] < 200:
            raise RuntimeError(f'{name} terminal crop lacks a dominant grid background or visible glyphs')
    if dark['luminance'] > 100 or light['luminance'] < 160 or light['luminance'] - dark['luminance'] < 75:
        raise RuntimeError('Appearance Light did not repaint the existing terminal grid background')
    if max(abs(a - b) for a, b in zip(dark['background'], restored['background'])) > 3:
        raise RuntimeError('Appearance Dark did not restore the existing terminal grid background')


def verify_font(comparison):
    first, second = comparison['before'], comparison['after']
    if min(first['backgroundShare'], second['backgroundShare']) < 0.65:
        raise RuntimeError('code-font evidence is not a settled terminal grid')
    if max(abs(a - b) for a, b in zip(first['background'], second['background'])) > 2:
        raise RuntimeError('code-font comparison also changed the terminal theme')
    if min(first['inkPixels'], second['inkPixels']) < 200:
        raise RuntimeError('code-font comparison lost the existing terminal output')
    if comparison['inkChangedPixels'] < max(30, comparison['inkUnionPixels'] * 0.03):
        raise RuntimeError('Appearance Code font did not change existing terminal glyph paint or metrics')


def verify_terminal_output(written, shell_command, copied):
    """Pair the completed PTY write with exact text copied from the emulator."""
    if MARKER in shell_command:
        raise RuntimeError('typed shell command contains the marker and could pass through echo')
    if written != PAINT.encode():
        raise RuntimeError('real PTY program did not finish writing the exact stable grid')
    if copied != MARKER:
        raise RuntimeError(f'terminal output or selection was lost: {copied!r}')


class TerminalAppearance:
    def __init__(self, client, env, output, fixture, proxy, clipboard):
        self.client, self.env, self.output = client, env, output
        self.fixture, self.proxy, self.clipboard = fixture, proxy, clipboard
        self.ui = AppearanceInteraction(client, env, output)
        self.preferences = Path(env['XDG_CONFIG_HOME']) / 'openchamber-native/appearance.json'
        self.probes = b''
        self.session = self.process = None
        self.capture = fixture.project / 'terminal-appearance-proof'
        self.capture.mkdir(mode=0o700)
        (self.capture / 'paint.bin').write_bytes(PAINT.encode())
        (self.capture / 'paint.py').write_text(PTY_PROGRAM)

    def click(self, key):
        self.ui.click(key)
        if self.client.substitutions:
            raise RuntimeError('pointer input required an accessibility substitute')

    def keys(self, *keys):
        self.client._activate()
        command(self.env, 'xdotool', 'key', '--clearmodifiers', *keys)

    def stored(self, key, value):
        return (self.preferences.is_file()
                and json.loads(self.preferences.read_text()).get('preferences', {}).get(key) == value)

    def appearance(self, initial=False):
        self.click('settings')
        self.ui.node('settings-dialog')
        if initial:
            self.click('settings-nav-general')
            until('General navigation did not open General',
                  lambda: self.ui.node('settings-current-page').get_name() == 'General', 10)
        self.click('settings-nav-appearance')
        until('Appearance navigation did not open Appearance',
              lambda: self.ui.node('settings-current-page').get_name() == 'Appearance', 10)

    def mode(self, mode):
        key = 'appearance-mode-' + mode
        self.click(key)
        until(f'{mode} mode did not expose selected semantics', lambda: self.ui.selected(key), 10)
        until(f'{mode} mode did not persist', lambda: self.stored('mode', mode), 15)

    def focus_terminal(self):
        # Clicking its existing tab focuses the surface without clearing its
        # selection. Clicking inside the grid would erase the proof itself.
        self.click('terminal-tab-1')
        until('existing terminal did not receive focus', lambda: self.ui.focused('terminal-surface'), 10)

    def terminals(self):
        query = urllib.parse.urlencode({'cwd': str(self.fixture.project)})
        value = self.fixture.get('/api/terminal/sessions?' + query)
        if not isinstance(value, dict) or not isinstance(value.get('sessions'), list):
            raise ValueError('server did not return an authoritative terminal list')
        return value['sessions']

    def start(self):
        if self.terminals():
            raise RuntimeError('fresh owned fixture already has terminals')
        self.keys('ctrl+j')
        self.ui.node('terminal-surface')
        sessions = until('server did not create exactly one running terminal',
                         lambda: self.terminals(), 20)
        if len(sessions) != 1 or sessions[0]['status'] != 'running':
            raise RuntimeError(f'expected one real running terminal: {sessions}')
        self.session = sessions[0]
        self.focus_terminal()
        line = 'python3 ' + shlex.quote(str(self.capture / 'paint.py')) + ' ' + shlex.quote(str(self.capture))
        command(self.env, 'xdotool', 'type', '--clearmodifiers', '--delay', '8', line, timeout=60)
        self.keys('Return')
        until('typed shell command did not write the stable grid through the real PTY',
              lambda: (self.capture / 'written.bin').is_file(), 45)
        self.process = json.loads((self.capture / 'process.json').read_text())
        for key in ('pid', 'parentPid'):
            self.process[key + 'Starttime'] = process_identity(self.process[key])
            if self.process[key + 'Starttime'] is None:
                raise RuntimeError('the terminal proof process did not remain alive')
        if not self.process['stdin'].startswith('/dev/pts/') or self.process['stdin'] != self.process['stdout']:
            raise RuntimeError('the shell command did not run with an owned real PTY')
        self.shell_command = line
        until('terminal socket did not attach', lambda: self.proxy.open_terminal_sockets() == 1, 15)
        self.creates, self.upgrades = len(self.proxy.terminal_commands('POST', '/create')), len(self.proxy.upgrades)
        if self.creates != 1 or self.upgrades != 1:
            raise RuntimeError('opening one terminal created or connected more than once')
        self.probe_input('before-appearance')
        # Wait for the actual grid paint before double-clicking its first word.
        self.capture_grid('initial-unselected')
        rect = self.client._rect(self.ui.node('terminal-surface'))
        center_x, center_y = self.client._screen_point(self.ui.node('terminal-surface'))
        x = round(center_x + (12 - rect[2] / 2) * self.client.scale)
        y = round(center_y + (12 - rect[3] / 2) * self.client.scale)
        command(self.env, 'xdotool', 'mousemove', '--sync', str(x), str(y))
        if self.client._pointer() != (x, y):
            raise RuntimeError('private pointer did not reach the terminal marker')
        command(self.env, 'xdotool', 'click', '--repeat', '2', '--delay', '90', '1')
        return self.checkpoint('dark-before')

    def retained(self, name):
        current = self.terminals()
        if current != [self.session]:
            raise RuntimeError(f'{name} changed terminal session identity, status or creation metadata: {current}')
        for key in ('pid', 'parentPid'):
            if process_identity(self.process[key]) != self.process[key + 'Starttime']:
                raise RuntimeError(f'{name} replaced or stopped the existing terminal {key}')
        if os.readlink(f"/proc/{self.process['pid']}/fd/1") != self.process['stdout']:
            raise RuntimeError('the proof process changed its real PTY')
        if (len(self.proxy.terminal_commands('POST', '/create')) != self.creates
                or len(self.proxy.upgrades) != self.upgrades or self.proxy.open_terminal_sockets() != 1
                or self.proxy.terminal_commands('POST', '/restart') or self.proxy.terminal_commands('DELETE')):
            raise RuntimeError('Appearance replaced or reconnected the existing terminal')
        # This reads the selected marker from the actual emulator. Selection
        # is never remade between opening Settings and this copy assertion.
        copied = self.clipboard.copy(lambda: self.keys('ctrl+shift+c'), timeout_ms=8000)
        written = (self.capture / 'written.bin').read_bytes()
        verify_terminal_output(written, self.shell_command, copied)
        if (self.capture / 'input.bin').read_bytes() != self.probes:
            raise RuntimeError(f'{name} sent unexpected input to the existing PTY')
        return {'session': current[0], 'process': self.process, 'copiedMarker': copied,
                'ptyInput': self.probes.decode(), 'ptyWrittenBytes': len(written),
                'createRequests': self.creates, 'socketUpgrades': self.upgrades}

    def probe_input(self, name):
        # Input intentionally clears selection. Probe before selecting the
        # marker and after every Appearance/selection/paint assertion is done.
        token = f'probe-{name}\n'.encode()
        self.probes += token
        command(self.env, 'xdotool', 'type', '--clearmodifiers', token[:-1].decode())
        self.keys('Return')
        until(f'{name} did not deliver exact input to the existing PTY process',
              lambda: (self.capture / 'input.bin').is_file()
              and (self.capture / 'input.bin').read_bytes() == self.probes, 15)

    def capture_grid(self, name):
        if any(node.get_accessible_id() == 'settings-dialog' for node in self.client._walk()):
            raise RuntimeError('Settings still overlays the terminal measurement')
        self.client._activate()
        # Keep hover menus and the pointer out of the measured grid.
        x, y = self.client._screen_point(self.ui.node('workspace'))
        command(self.env, 'xdotool', 'mousemove', '--sync', str(x), str(round(y - self.client.height * self.client.scale / 2 + 2)))
        previous, since = None, time.monotonic()
        deadline = time.monotonic() + 15
        full_path = self.output / f'{name}.png'
        while time.monotonic() < deadline:
            self.client.screenshot(full_path)
            image = png.read(full_path)
            rect = self.client._rect(self.ui.node('terminal-surface'))
            bounds = grid_crop(rect, self.client.scale, (image.width, image.height))
            grid = png.crop(image, *bounds)
            signature = (bounds, grid.rgba)
            if signature != previous:
                previous, since = signature, time.monotonic()
            elif time.monotonic() - since >= 0.75:
                glyph_bounds = grid_crop(rect, self.client.scale, (image.width, image.height), glyphs=True)
                glyphs = png.crop(image, *glyph_bounds)
                png.write(self.output / f'{name}-grid.png', grid)
                png.write(self.output / f'{name}-glyphs.png', glyphs)
                return {'image': grid, 'glyphs': glyphs, 'geometry': rect, 'crop': bounds,
                        'glyphCrop': glyph_bounds, 'metrics': paint_metrics(grid)}
            self.client._read_marks(0.15)
        raise TimeoutError(f'{name} terminal interior did not hold its paint still for 750 ms')

    def checkpoint(self, name):
        self.focus_terminal()
        retained = self.retained(name)
        painted = self.capture_grid(name)
        return painted, retained

    def font_options(self):
        self.click('appearance-mono-font')
        prefix = 'appearance-mono-font-option-'
        self.ui.node(prefix + 'system-mono')
        options = [(node.get_accessible_id()[len(prefix):], node.get_name()) for node in self.client._walk()
                   if (node.get_accessible_id() or '').startswith(prefix)]
        self.keys('Escape')
        until('code font picker did not restore focus', lambda: self.ui.focused('appearance-mono-font'), 10)
        return options

    def font(self, value):
        trigger = 'appearance-mono-font'
        self.click(trigger)
        self.click(trigger + '-option-' + value)
        until('code font did not persist', lambda: self.stored('monoFont', value), 15)
        until('code font picker did not restore trigger focus', lambda: self.ui.focused(trigger), 10)


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    report = {'passed': False, 'status': 'failed', 'binary': identity, 'display': 'owned-xvfb',
              'viewport': [args.width, args.height], 'scale': args.scale,
              'limitations': ['Native-only runtime evidence; no matched Electron/native pixel parity.',
                              'Source-level selection-preservation tests belong to the terminal implementation change.']}
    theme = {'name': 'existing-terminal-live-theme', 'passed': False, 'status': 'not-run'}
    font = {'name': 'existing-terminal-live-code-font', 'passed': False, 'status': 'not-run'}
    report['checks'] = [theme, font]
    clients, proxy, driver, captures = [], None, None, {}
    previous = accessibility_enabled()
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            virtual_display.verify_owned_display()
            root = Path('/tmp/opencode') / ('terminal-appearance-' + uuid.uuid4().hex)
            if root.parent != Path('/tmp/opencode') or root.exists():
                raise ValueError('fixture root must be new and directly under /tmp/opencode')
            fixture = cleanup.enter_context(HostFixture(root, args.output / 'fixture', args, interrupts))
            session = fixture.create_session('stream-1200cps')
            report['fixture'] = {'root': str(root), 'project': str(fixture.project), 'sessionId': session['id']}
            proxy = TerminalProxy(fixture.url)
            cleanup.callback(proxy.stop)
            env = native_environment(args.output / 'native-state', proxy.url)
            for folder in runner.state_dirs(args.output / 'native-state').values():
                Path(folder).mkdir(parents=True, mode=0o700, exist_ok=True)
            clipboard = ClipboardProbe()
            log = cleanup.enter_context((args.output / 'native.log').open('w'))
            virtual_display.verify_owned_display()
            client = NativeClient(args.binary, env, args.scale, args.width, args.height, stderr=log)
            clients.append(client)
            cleanup.callback(client.close)

            def last_state():
                if client.process is not None and client.process.poll() is None:
                    try:
                        client.screenshot(args.output / 'last-state.png')
                    except Exception as error:
                        report.setdefault('captureErrors', []).append(str(error))
            cleanup.callback(last_state)
            client.launch()
            client.wait_ready(45, connected=True)
            key = until('owned fixture session did not appear', lambda: next((row['key'] for row in client.rows()
                        if row['key'].endswith(':session:' + session['id'])), None), 30)
            client.click(key)
            until('owned fixture session did not select', lambda: any(row['selected']
                  and row['key'].endswith(':session:' + session['id']) for row in client.rows()), 15)
            driver = TerminalAppearance(client, env, args.output, fixture, proxy, clipboard)
            theme['status'] = 'running'
            driver.appearance(initial=True)
            driver.mode('dark')
            if not driver.stored('monoFont', 'system-mono'):
                raise RuntimeError('fresh private profile did not start with the System monospace code font')
            driver.ui.close()
            dark, retained = driver.start()
            report['shellCommand'] = driver.shell_command
            report['retention'] = {'dark-before': retained}
            captures['dark-before'] = dark

            for mode, name in (('light', 'light-after'), ('dark', 'dark-restored')):
                driver.appearance()
                driver.mode(mode)
                driver.ui.close()
                captures[name], report['retention'][name] = driver.checkpoint(name)
            theme['darkToLight'] = compare_paint(dark['image'], captures['light-after']['image'], args.output / 'theme-dark-light-diff.png')
            theme['lightToRestored'] = compare_paint(captures['light-after']['image'], captures['dark-restored']['image'], args.output / 'theme-light-dark-diff.png')
            try:
                verify_theme(dark['metrics'], captures['light-after']['metrics'], captures['dark-restored']['metrics'])
                theme.update(passed=True, status='passed')
            except RuntimeError as error:
                theme.update(status='failed', error=str(error), traceback=traceback.format_exc())

            # Font entry is independent of theme repaint. Record the completed
            # theme result before opening any font picker on the existing PTY.
            font['status'] = 'running'
            driver.appearance()
            options = driver.font_options()
            report['fontOptions'] = options
            installed = set(command(env, 'fc-list', '--format=%{family[0]}\n').splitlines())
            system = next((name for name in ('SFMono-Regular', 'Menlo', 'Cascadia Mono', 'Segoe UI Mono')
                           if name in installed), command(env, 'fc-match', '--format=%{family[0]}', 'monospace').strip())
            baseline = ('system-mono', system)
            candidate = next(((value, label) for value, label in options
                              if value != baseline[0] and label != system), None)
            font.update(baseline=baseline, candidate=candidate)
            if candidate is None:
                font.update(status='unavailable', reason='The real picker has no installed code font distinct from the baseline family.')
                report['limitations'].append(font['reason'])
                driver.ui.close()
                driver.focus_terminal()
            else:
                driver.font(candidate[0])
                if not driver.stored('mode', 'dark'):
                    raise RuntimeError('code-font comparison did not keep the selected theme fixed')
                driver.ui.close()
                changed, report['retention']['font-after'] = driver.checkpoint('font-after')
                captures['font-after'] = changed
                font['comparison'] = compare_paint(captures['dark-restored']['glyphs'], changed['glyphs'], args.output / 'font-glyph-diff.png')
                driver.appearance()
                driver.font(baseline[0])
                driver.ui.close()
                restored, report['retention']['font-restored'] = driver.checkpoint('font-restored')
                captures['font-restored'] = restored
                font['restoreComparison'] = compare_paint(changed['glyphs'], restored['glyphs'], args.output / 'font-restore-diff.png')
                font['baselineRestore'] = compare_paint(captures['dark-restored']['glyphs'], restored['glyphs'], args.output / 'font-baseline-restored-diff.png')
                try:
                    verify_font(font['comparison'])
                    verify_font(font['restoreComparison'])
                    if font['baselineRestore']['changedPixels']:
                        raise RuntimeError('restoring Code font did not restore the original stationary glyph paint')
                    font.update(passed=True, status='passed')
                except RuntimeError as error:
                    font.update(error=str(error), status='failed', traceback=traceback.format_exc())
            report['inputSubstitutions'] = client.substitutions
            if client.substitutions:
                raise RuntimeError('pointer input required an accessibility substitute')
            driver.probe_input('after-appearance')
            report['finalPtyProbe'] = {'input': driver.probes.decode(), 'process': driver.process}
        report['passed'] = all(check['passed'] for check in report['checks'])
        report['status'] = 'passed' if report['passed'] else ('incomplete' if font.get('status') == 'unavailable' and theme['passed'] else 'failed')
    except Exception as error:
        traceback.print_exc()
        report['error'] = str(error)
        report['traceback'] = traceback.format_exc()
        for check in report['checks']:
            if check['status'] == 'running':
                check.update(status='failed', error=str(error), traceback=report['traceback'])
    finally:
        report['captures'] = {name: {key: value for key, value in capture.items() if key not in ('image', 'glyphs')}
                              for name, capture in captures.items()}
        try:
            set_accessibility(previous)
        except Exception as error:
            report.update(passed=False, status='failed', cleanupError=str(error))
        try:
            if binary_info(args.binary) != identity:
                report.update(passed=False, status='failed', error='the frozen binary changed during qualification')
        except OSError as error:
            report.update(passed=False, status='failed', error=str(error))
        report['inputSubstitutions'] = [item for client in clients for item in client.substitutions]
        report['nativeProcesses'] = [{'pid': client.process.pid, 'exitCode': client.process.poll()}
                                     for client in clients if client.process is not None]
        if any(item['exitCode'] is None for item in report['nativeProcesses']):
            report.update(passed=False, status='failed', cleanupError='native process remained after cleanup')
        if driver and driver.process:
            try:
                for source, target in (('written.bin', 'pty-output.bin'), ('input.bin', 'pty-input.bin'),
                                       ('process.json', 'pty-process.json')):
                    if (driver.capture / source).is_file():
                        (args.output / target).write_bytes((driver.capture / source).read_bytes())
            except OSError as error:
                report.update(passed=False, status='failed', captureError=str(error))
            report['ptyProcesses'] = [{'pid': driver.process[key], 'starttime': driver.process[key + 'Starttime'],
                                      'stillOwned': process_identity(driver.process[key]) == driver.process[key + 'Starttime']}
                                     for key in ('pid', 'parentPid') if key + 'Starttime' in driver.process]
            if any(item['stillOwned'] for item in report['ptyProcesses']):
                report.update(passed=False, status='failed', cleanupError='owned PTY process remained after fixture cleanup')
        if proxy:
            report['terminalRequests'] = proxy.terminal_commands()
            report['terminalSocketUpgrades'] = len(proxy.upgrades)
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(f"{report['status']}: {args.output / 'report.json'}", flush=True)
    return 0 if report['passed'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--dist', type=Path, default=runner.DEFAULT_DIST)
    parser.add_argument('--opencode-binary', type=Path, default=runner.DEFAULT_OPENCODE)
    parser.add_argument('--scale', type=float, choices=(1, 1.25, 1.5, 2), default=1.25)
    parser.add_argument('--width', type=int, default=1280)
    parser.add_argument('--height', type=int, default=800)
    args = parser.parse_args()
    for key in ('binary', 'output', 'dist', 'opencode_binary'):
        setattr(args, key, getattr(args, key).resolve())
    try:
        validate_output(args.output)
    except ValueError as error:
        parser.error(str(error))
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error('--binary must name a frozen executable built before this run')
    if args.width < 800 or args.height < 520:
        parser.error('viewport must meet the native 800 by 520 minimum')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(args.width * args.scale), round(args.height * args.scale))
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
