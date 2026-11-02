#!/usr/bin/env python3
"""Exercise native Appearance and Settings on an owned private Xvfb display."""

import argparse
from collections import Counter
from contextlib import ExitStack
import json
import math
from pathlib import Path
import stat
import sys
import traceback
import uuid

from clipboard_probe import ClipboardProbe
from native_adapter import NativeClient, _atspi
from native_hosts import HostFixture
from native_interactions import Interaction, Interrupts, accessibility_enabled, set_accessibility, until
from native_platform import binary_info, command, editable_text
import png
import runner
import virtual_display


DRAFT = 'Appearance must retain this draft. العربية עברית 你好'


def background_luminance(path):
    """Read the most common painted color, excluding alpha from the count."""
    image = png.read(path)
    colors = Counter(tuple(image.rgba[index:index + 3]) for index in range(0, len(image.rgba), 64))
    color = colors.most_common(1)[0][0]
    return sum(channel * weight for channel, weight in zip(color, (0.2126, 0.7152, 0.0722)))


def validate_output(path):
    allowed = (Path('/tmp/opencode'), runner.REPO_ROOT / 'native/target/tmp')
    if path.parent not in allowed or path.exists():
        raise ValueError('--output must be new and directly under /tmp/opencode or native/target/tmp')


def appearance_title(locale):
    resources = json.loads((runner.REPO_ROOT / 'native/crates/desktop/resources/locales.json').read_text())
    return resources['locales'][locale]['messages']['settings.page.appearance.title']


def native_environment(state, server_url):
    env = runner.native_env(state, server_url, 'openchamber-light')
    # Both launches must observe saved preferences without development overrides.
    env.pop('OPENCHAMBER_NATIVE_THEME', None)
    env.pop('OPENCHAMBER_NATIVE_LOCALE', None)
    return env


def verify_text_growth(before, after):
    if before <= 0 or not 1.3 <= after / before <= 1.7:
        raise RuntimeError(f'150% text size did not scale the visible heading: {before} -> {after}')


def verify_text_restore(before, restored):
    if before <= 0 or restored <= 0 or abs(restored - before) > 1:
        raise RuntimeError(f'100% text size did not restore the visible heading: {before} -> {restored}')


def verify_maximum_text_growth(before, after):
    if before <= 0 or not 1.8 <= after / before <= 2.2:
        raise RuntimeError(f'200% text size did not scale the visible heading: {before} -> {after}')


def verify_scroll_frame(before, after):
    for key, bounds in before.items():
        current = after[key]
        if any(abs(old - new) > 1 for old, new in zip(bounds, current)):
            raise RuntimeError(f'Page scrolling moved Settings chrome: {key}, {bounds} -> {current}')


def settings_scroll_direction(rect, frame):
    """AT-SPI geometry must fit its clipping frame before a pointer click."""
    if (len(rect) != 4 or len(frame) != 4
            or not all(math.isfinite(value) for value in (*rect, *frame))
            or min(rect[2:]) <= 0 or min(frame[2:]) <= 0):
        raise ValueError('Settings control or clipping frame has invalid geometry')
    x, y, width, height = rect
    left, top, frame_width, frame_height = frame
    if x < left - 1 or x + width > left + frame_width + 1 or height > frame_height + 1:
        raise RuntimeError('Settings control cannot fit inside its clipping frame')
    if y < top - 1:
        return -1
    if y + height > top + frame_height + 1:
        return 1
    return 0


class AppearanceInteraction:
    def __init__(self, client, env, output):
        self.client, self.env, self.output = client, env, output

    def node(self, key):
        if not self.client.wait_for({'elements': [key]}, 15):
            raise RuntimeError(f'control did not appear: {key}')
        return self.client._find(key)

    def focused(self, key):
        node = self.client._find(key)
        node.clear_cache()
        return node.get_state_set().contains(_atspi().StateType.FOCUSED)

    def selected(self, key):
        node = self.node(key)
        node.clear_cache()
        states = node.get_state_set()
        return any(states.contains(state) for state in (_atspi().StateType.CHECKED, _atspi().StateType.SELECTED))

    def visible(self, key):
        node = self.node(key)
        node.clear_cache()
        rect = self.client._rect(node)
        if settings_scroll_direction(rect, self.clipping_frame(key)):
            raise RuntimeError(f'control is outside its clipping frame: {key}, {rect}')
        return rect

    def clipping_frame(self, key):
        if key.startswith(('appearance-', 'settings-')) and '-option-' not in key:
            node = self.node('settings-dialog')
            node.clear_cache()
            return self.client._rect(node)
        return [0, 0, self.client.width, self.client.height]

    def click(self, key):
        self.reveal(key)
        self.client.click(key)

    def reveal(self, key):
        for _ in range(36):
            try:
                return self.visible(key)
            except RuntimeError:
                rect = self.client._rect(self.node(key))
                frame = self.clipping_frame(key)
                direction = settings_scroll_direction(rect, frame)
                self.client._activate()
                center_x, center_y = self.client._screen_point(self.node('workspace'))
                logical_x = max(frame[0] + 24, min(frame[0] + frame[2] - 24, rect[0] + rect[2] / 2))
                logical_y = frame[1] + frame[3] * 0.6
                x = round(center_x + (logical_x - self.client.width / 2) * self.client.scale)
                y = round(center_y + (logical_y - self.client.height / 2) * self.client.scale)
                wheel = '4' if direction < 0 else '5'
                command(self.env, 'xdotool', 'mousemove', '--sync', str(x), str(y))
                if self.client._pointer() != (x, y):
                    raise RuntimeError('private pointer did not reach the Settings scroll area')
                command(self.env, 'xdotool', 'click', '--repeat', '2', '--delay', '80', wheel)
                self.client._read_marks(0.15)
        raise RuntimeError(f'pointer wheel did not reveal the Settings control: {key}')

    def keyboard(self, key):
        self.reveal(key)
        node = self.node(key)
        if not node.get_component_iface().grab_focus():
            raise RuntimeError(f'control cannot receive keyboard focus: {key}')
        until(f'control did not focus: {key}', lambda: self.focused(key), 5)
        command(self.env, 'xdotool', 'key', '--clearmodifiers', 'space')

    def filter_locale(self, locale):
        def focused_search():
            for node in self.client._walk():
                states = node.get_state_set()
                if (node.get_accessible_id() != 'settings-search'
                        and states.contains(_atspi().StateType.FOCUSED)
                        and states.contains(_atspi().StateType.EDITABLE)):
                    return node
            return None

        search = until('Language popup did not focus its search field', focused_search, 10)
        search_role = search.get_role_name()
        command(self.env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a')
        command(self.env, 'xdotool', 'type', '--clearmodifiers', locale)
        self.node('appearance-locale-option-' + locale)
        return search_role

    def keyboard_locale(self, locale):
        self.keyboard('appearance-locale')
        search_role = self.filter_locale(locale)
        key = 'appearance-locale-option-' + locale
        until('Language search did not filter to the requested locale', lambda: [
            node.get_accessible_id() for node in self.client._walk()
            if node.get_accessible_id().startswith('appearance-locale-option-')
        ] == [key], 10)
        command(self.env, 'xdotool', 'key', '--clearmodifiers', 'Down', 'Return')
        return {'searchRole': search_role, 'query': locale, 'selectedOption': key}

    def open(self):
        self.click('settings')
        self.node('settings-dialog')

    def close(self):
        command(self.env, 'xdotool', 'key', '--clearmodifiers', 'Escape')
        until('Settings did not close', lambda: not any(node.get_accessible_id() == 'settings-dialog' for node in self.client._walk()), 10)

    def capture(self, name):
        path = self.output / f'{name}.png'
        self.client.screenshot(path)
        return path


def qualify_text_scale(ui, preferences, report):
    key = 'appearance-font-size'

    def reveal(target):
        fixed = ('settings-dialog', 'settings-navigation', 'settings-search')
        before = {anchor: ui.visible(anchor) for anchor in fixed}
        ui.reveal(target)
        after = {anchor: ui.visible(anchor) for anchor in fixed}
        verify_scroll_frame(before, after)
        report.setdefault('scrollFrames', []).append({'target': target, 'before': before, 'after': after})

    def stored(value):
        return json.loads(preferences.read_text())['preferences']['fontSize'] == value

    def retained(value):
        return ui.focused(key) and editable_text(ui.node(key)) == str(value)

    def enter(value):
        command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a')
        command(ui.env, 'xdotool', 'type', '--clearmodifiers', str(value))
        command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'Return')

    ui.click('settings-search')
    command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a')
    command(ui.env, 'xdotool', 'type', '--clearmodifiers', 'Interface font size')
    ui.click('settings-result-appearance-interface-font-size')
    until('Text-size search did not focus its retained input', lambda: retained(100), 10)
    reveal('settings-current-page')
    baseline = ui.visible('settings-current-page')[3]
    ui.capture('font-size-100-heading')
    if not retained(100):
        raise RuntimeError('Scrolling the text-size page lost input focus or its value')
    reveal(key)
    enter(150)
    until('150% text size did not persist', lambda: stored(150), 15)
    until('150% apply lost the retained input focus or value', lambda: retained(150), 10)
    ui.capture('font-size-150-input')
    reveal('settings-current-page')
    enlarged = ui.visible('settings-current-page')[3]
    verify_text_growth(baseline, enlarged)
    ui.capture('font-size-150-heading')
    if not retained(150):
        raise RuntimeError('150% reflow lost input focus or its value')
    reveal(key)
    enter(100)
    until('Restored 100% text size did not persist', lambda: stored(100), 15)
    until('Restoring 100% lost the retained input focus or value', lambda: retained(100), 10)
    reveal('settings-current-page')
    restored = ui.visible('settings-current-page')[3]
    verify_text_restore(baseline, restored)
    ui.capture('font-size-restored-heading')
    report['textScale'] = {'headingHeight100': baseline, 'headingHeight150': enlarged,
                           'headingHeightRestored': restored}
    report['checks'].extend(['font-size-search-focus', 'font-size-150-live-text-geometry',
                             'font-size-apply-retains-input-focus-and-value', 'font-size-restores-100'])
    if not retained(100):
        raise RuntimeError('100% reflow lost input focus or its value')
    reveal(key)
    enter(200)
    until('200% text size did not persist', lambda: stored(200), 15)
    until('200% apply lost the retained input focus or value', lambda: retained(200), 10)
    reveal(key)
    ui.visible(key)
    ui.capture('font-size-200-input')
    reveal('settings-current-page')
    maximum = ui.visible('settings-current-page')[3]
    verify_maximum_text_growth(baseline, maximum)
    ui.capture('font-size-200-page-top')
    if not retained(200):
        raise RuntimeError('200% reflow lost input focus or its value')
    reveal(key)
    enter(100)
    until('Restoring 100% from maximum text size did not persist', lambda: stored(100), 15)
    until('Restoring 100% from maximum lost input focus or value', lambda: retained(100), 10)
    reveal('settings-current-page')
    restored_maximum = ui.visible('settings-current-page')[3]
    verify_text_restore(baseline, restored_maximum)
    ui.capture('font-size-restored-from-200-heading')
    report['textScale'].update(headingHeight200=maximum, headingHeightRestoredFrom200=restored_maximum)
    report['checks'].extend(['font-size-200-live-text-geometry-and-visible-input',
                             'font-size-200-restores-100'])
    reveal(key)
    unchanged = preferences.read_bytes()
    enter(999)
    until('Invalid text size did not retain its input', lambda: retained(999), 10)
    until('Invalid text size did not expose validation', lambda: bool(ui.node('appearance-error').get_name()), 10)
    reveal('appearance-error')
    ui.visible('appearance-error')
    ui.capture('font-size-invalid-999')
    if preferences.read_bytes() != unchanged or not stored(100):
        raise RuntimeError('Invalid text-size input changed the persisted appearance')
    if not retained(999):
        raise RuntimeError('Revealing validation discarded the invalid input or focus')
    command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'Escape')
    ui.node('settings-dialog')
    until('Escape did not restore the saved text size in the retained input', lambda: retained(100), 10)
    until('Escape did not clear text-size validation', lambda: not ui.node('appearance-error').get_name(), 10)
    if preferences.read_bytes() != unchanged:
        raise RuntimeError('Cancelling invalid text size changed the saved appearance')
    report['checks'].extend(['font-size-invalid-draft-visible-validation-and-escape',
                             'page-scrolling-keeps-settings-navigation-and-dialog-fixed'])
    ui.click('settings-search')
    command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a', 'BackSpace')


def qualify_installed_font(ui, preferences, report):
    trigger = 'appearance-ui-font'
    prefix = trigger + '-option-'
    ui.click(trigger)
    ui.node(prefix + 'system')
    candidates = [(node.get_accessible_id(), node.get_name()) for node in ui.client._walk()
                  if (node.get_accessible_id() or '').startswith(prefix)
                  and node.get_accessible_id() != prefix + 'system' and node.get_name()]
    if not candidates:
        command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'Escape')
        report['installedFont'] = {'status': 'unavailable', 'reason': 'No named installed-font option appeared in the real picker.'}
        return
    key, label = candidates[0]
    value = key[len(prefix):]
    ui.click(key)
    until('Installed font selection did not persist', lambda: json.loads(preferences.read_text())['preferences']['uiFont'] == value, 15)
    until('Font selection did not restore trigger focus', lambda: ui.focused(trigger), 10)
    ui.capture('installed-font-applied')
    ui.click(trigger)
    until('Reopened font picker lost its selected font', lambda: ui.selected(key), 10)
    command(ui.env, 'xdotool', 'key', '--clearmodifiers', 'Escape')
    report['installedFont'] = {'status': 'passed', 'value': value, 'label': label, 'option': key}
    report['checks'].append('named-installed-font-selection-and-persistence')


def qualify(args):
    virtual_display.verify_owned_display()
    args.output.mkdir(mode=0o700)
    identity = binary_info(args.binary)
    checks, clients = [], []
    report = {'passed': False, 'binary': identity, 'display': 'owned-xvfb', 'checks': checks,
              'viewport': [args.width, args.height], 'scale': args.scale,
              'limitations': ['No Electron/native pixel parity comparison.',
                              'No system desktop theme-change event is injected.',
                              'Transcript scroll retention requires a populated transcript; this run checks Settings scroll.']}
    previous = accessibility_enabled()
    try:
        set_accessibility(True)
        with Interrupts() as interrupts, ExitStack() as cleanup:
            fixture = cleanup.enter_context(HostFixture(Path('/tmp/opencode') / f'appearance-{uuid.uuid4().hex}', args.output / 'fixture', args, interrupts))
            session = fixture.create_session('stream-1200cps')
            env = native_environment(args.output / 'state', fixture.url)
            preferences = Path(env['XDG_CONFIG_HOME']) / 'openchamber-native/appearance.json'

            def launch(name):
                log = cleanup.enter_context((args.output / f'{name}.log').open('w'))
                client = NativeClient(args.binary, env, args.scale, args.width, args.height, stderr=log)
                cleanup.callback(client.close)
                clients.append(client)

                def last_state():
                    if client.process is not None and client.process.poll() is None:
                        try:
                            client.screenshot(args.output / f'{name}-last-state.png')
                        except Exception as error:
                            report.setdefault('captureErrors', []).append(str(error))
                cleanup.callback(last_state)
                client.launch()
                client.wait_ready(45, connected=True)
                key = until('fixture session did not appear', lambda: next((row['key'] for row in client.rows() if row['key'].endswith(':session:' + session['id'])), None), 30)
                client.click(key)
                until('fixture session did not select', lambda: any(row['selected'] and row['key'].endswith(':session:' + session['id']) for row in client.rows()), 15)
                probe = object.__new__(Interaction)
                probe.client, probe.env, probe.clipboard = client, env, ClipboardProbe()
                until('composer unavailable', probe.composer, 20)
                return AppearanceInteraction(client, env, args.output), probe

            ui, probe = launch('first')
            probe.paste(probe.composer(), DRAFT)
            until('draft not entered', lambda: editable_text(probe.composer()) == DRAFT, 10)
            ui.open()
            ui.click('settings-nav-general')
            until('General navigation opened another page', lambda: ui.node('settings-current-page').get_name() == 'General', 10)
            ui.client.activate('settings-nav-remote-instances')
            until('Remote instances navigation opened another page', lambda: ui.node('settings-current-page').get_name() == 'Remote instances', 10)
            ui.click('settings-nav-appearance')
            ui.node('appearance-mode-system')
            if not ui.selected('appearance-mode-system'):
                raise RuntimeError('Fresh private profile did not start in System mode')
            if ui.node('settings-current-page').get_name() != 'Appearance':
                raise RuntimeError('Appearance navigation did not open the reference page')
            checks.append('footer-settings-and-appearance-navigation')
            checks.append('general-and-remote-instances-navigation')

            def saved_mode(mode):
                return preferences.is_file() and json.loads(preferences.read_text()).get('preferences', {}).get('mode') == mode

            for mode, activation in [('light', ui.click), ('dark', ui.keyboard), ('system', ui.client.activate), ('light', ui.click)]:
                key = 'appearance-mode-' + mode
                activation(key)
                until(f'{mode} did not expose selected semantics', lambda: ui.selected(key), 10)
                until(f'{mode} did not persist', lambda: saved_mode(mode), 15)
                if mode == 'dark' and not ui.focused(key):
                    raise RuntimeError('Applying dark mode lost keyboard focus')
                ui.capture('mode-' + mode)
            light = background_luminance(args.output / 'mode-light.png')
            dark = background_luminance(args.output / 'mode-dark.png')
            report['paintLuminance'] = {'light': light, 'dark': dark}
            if light - dark < 75:
                raise RuntimeError('Light/Dark selected state did not produce distinct light/dark paint')
            checks.extend(['pointer-light', 'keyboard-dark', 'accessible-system', 'live-theme-paint', 'mode-persistence'])

            qualify_text_scale(ui, preferences, report)
            qualify_installed_font(ui, preferences, report)

            locale_before_search = ui.client._rect(ui.node('appearance-locale'))
            ui.click('settings-search')
            command(env, 'xdotool', 'type', '--clearmodifiers', 'Language')
            ui.click('settings-result-appearance-language')
            until('Language search did not focus its control', lambda: ui.focused('appearance-locale'), 10)
            if editable_text(ui.node('settings-search')) != 'Language':
                raise RuntimeError('Search navigation discarded the query')
            ui.visible('appearance-locale')
            report['searchScroll'] = {'before': locale_before_search, 'after': ui.visible('appearance-locale')}
            if abs(report['searchScroll']['before'][1] - report['searchScroll']['after'][1]) > 1:
                checks.append('search-scrolls-language-into-view')
            else:
                report['limitations'].append('Language was already visible; search did not exercise a nonzero scroll offset.')
            checks.append('search-language-target-focus-and-query-retention')
            ui.click('settings-search')
            command(env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a', 'BackSpace')

            for locale, method in [('ar', 'pointer'), ('he', 'keyboard'), ('en', 'accessibility'), ('he', 'pointer')]:
                trigger_before = ui.visible('appearance-locale')
                if method == 'keyboard':
                    report['keyboardLanguage'] = ui.keyboard_locale(locale)
                else:
                    activation = ui.click if method == 'pointer' else ui.client.activate
                    activation('appearance-locale')
                    ui.filter_locale(locale)
                    ui.node('appearance-locale-option-' + locale)
                    activation('appearance-locale-option-' + locale)
                until(f'{locale} did not persist', lambda: preferences.is_file() and json.loads(preferences.read_text()).get('preferences', {}).get('locale') == locale, 15)
                until('Language selection did not restore trigger focus', lambda: ui.focused('appearance-locale'), 10)
                trigger_after = ui.visible('appearance-locale')
                if abs(trigger_before[1] - trigger_after[1]) > 1:
                    raise RuntimeError('Language apply changed the Settings scroll position')
                label = ui.node('settings-current-page').get_name()
                if label != appearance_title(locale):
                    raise RuntimeError(f'{locale} selection did not translate the existing Settings page')
                ui.capture('locale-' + locale)
                checks.append('live-locale-' + locale)

            ui.close()
            until('Settings close did not restore composer focus', lambda: probe.composer().get_state_set().contains(_atspi().StateType.FOCUSED), 10)
            if editable_text(probe.composer()) != DRAFT:
                raise RuntimeError('Appearance apply discarded the composer draft')
            ui.open()
            ui.node('appearance-locale')
            checks.extend(['close-restores-composer-focus', 'draft-retention', 'reopen-retains-appearance-page'])
            # Search still filters the navigation. Clear it before mode/failure checks.
            ui.click('settings-search')
            command(env, 'xdotool', 'key', '--clearmodifiers', 'ctrl+a', 'BackSpace')
            ui.click('settings-nav-appearance')
            before = preferences.read_bytes()
            directory = preferences.parent
            mode = stat.S_IMODE(directory.stat().st_mode)
            try:
                directory.chmod(0o500)
                ui.click('appearance-mode-dark')
                ui.node('appearance-error')
                ui.reveal('appearance-error')
                ui.reveal('appearance-retry')
                if not ui.selected('appearance-mode-dark'):
                    raise RuntimeError('Persistence failure discarded the current mode edit')
                if preferences.read_bytes() != before:
                    raise RuntimeError('Failed persistence changed the saved snapshot')
                ui.capture('failed-save')
            finally:
                directory.chmod(mode)
            ui.click('appearance-retry')
            until('Appearance persistence retry failed', lambda: saved_mode('dark'), 15)
            until('Retry did not preserve focus inside Settings', lambda: ui.focused('appearance-mode-dark'), 10)
            checks.extend(['failed-save-retains-edit-and-snapshot', 'visible-save-retry', 'retry-keeps-modal-focus'])
            ui.close()
            ui.client.close()
            ui, probe = launch('restart')
            until('restart did not restore composer draft', lambda: editable_text(probe.composer()) == DRAFT, 15)
            ui.open()
            ui.click('settings-nav-appearance')
            until('restart lost dark mode', lambda: ui.selected('appearance-mode-dark'), 10)
            if ui.node('settings-current-page').get_name() != appearance_title('he'):
                raise RuntimeError('Restart lost the selected Hebrew locale')
            if report['installedFont']['status'] == 'passed':
                ui.click('appearance-ui-font')
                until('Restart lost the selected installed font', lambda: ui.selected(report['installedFont']['option']), 10)
                command(env, 'xdotool', 'key', '--clearmodifiers', 'Escape')
                checks.append('restart-installed-font-selection')
            restart = ui.capture('restart-dark')
            if background_luminance(restart) > dark + 30:
                raise RuntimeError('Restart selected dark mode without applying its palette')
            checks.extend(['restart-mode-and-locale-persistence', 'restart-draft-retention'])
            report['inputSubstitutions'] = [item for client in clients for item in client.substitutions]
            if report['inputSubstitutions']:
                raise RuntimeError('pointer input required an accessibility substitute')
        report['passed'] = True
    except Exception as error:
        traceback.print_exc()
        report['error'] = str(error)
    finally:
        try:
            set_accessibility(previous)
        except Exception as error:
            report.update(passed=False, cleanupError=str(error))
        if binary_info(args.binary) != identity:
            report.update(passed=False, error='candidate changed during qualification')
        report['inputSubstitutions'] = [item for client in clients for item in client.substitutions]
        report['nativeProcesses'] = [{'pid': client.process.pid, 'exitCode': client.process.poll()} for client in clients if client.process is not None]
        if any(item['exitCode'] is None for item in report['nativeProcesses']):
            report.update(passed=False, cleanupError='native client remained after cleanup')
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
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
    if args.width < 800 or args.height < 520:
        parser.error('viewport must meet the native 800 by 520 minimum')
    if not virtual_display.inside():
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]], round(args.width * args.scale), round(args.height * args.scale))
    return qualify(args)


if __name__ == '__main__':
    raise SystemExit(main())
