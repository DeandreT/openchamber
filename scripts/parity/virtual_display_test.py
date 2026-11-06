"""Real private-display ownership and cleanup checks."""

import fcntl
import json
import os
import select
import signal
from pathlib import Path
import subprocess
import sys
import time
import unittest
from unittest.mock import MagicMock, Mock, patch

import virtual_display


class DisplayOwnershipTest(unittest.TestCase):
    def test_attached_pid_reuse_does_not_signal_replacement(self):
        pid = 424242
        address = 'unix:path=/private-fixture/bus'
        wanted = f'DBUS_SESSION_BUS_ADDRESS={address}'.encode()
        generation = 1
        clock_calls = 0
        owned_signals = []
        replacement_signals = []
        descriptors = {}
        closed = []
        entry = MagicMock()
        entry.name = str(pid)
        entry.stat.return_value.st_uid = os.getuid()
        environment = Mock()
        environment.read_bytes.side_effect = lambda: wanted if generation == 1 else b'UNRELATED=1'
        process_stat = Mock()
        process_stat.read_text.side_effect = lambda: f'{pid} (private fixture) ' + ' '.join(['S'] + ['0'] * 18 + [str(generation)])
        entry.__truediv__.side_effect = lambda name: {'environ': environment, 'stat': process_stat}[name]
        proc = Mock()
        proc.iterdir.return_value = [entry]

        def path(value):
            return {'/proc': proc, f'/proc/{pid}': entry}[str(value)]

        def clock():
            nonlocal generation, clock_calls
            clock_calls += 1
            if clock_calls == 1:
                return 0
            generation = 2
            return virtual_display.SHUTDOWN_GRACE_S + 1

        def bare_signal(target, value):
            self.assertEqual(target, pid)
            (owned_signals if generation == 1 else replacement_signals).append(value)

        def open_pidfd(target, flags=0):
            self.assertEqual(target, pid)
            descriptors[91001] = generation
            return 91001

        def bound_signal(descriptor, value, *_):
            if descriptors[descriptor] != generation:
                raise ProcessLookupError('the originally bound fixture process exited')
            owned_signals.append(value)

        select_module = Mock()
        select_module.poll.return_value.poll.return_value = []
        with patch('virtual_display.Path', side_effect=path), \
             patch('virtual_display.time.monotonic', side_effect=clock), \
             patch('virtual_display.time.sleep'), \
             patch('virtual_display.os.kill', side_effect=bare_signal), \
             patch('virtual_display.os.pidfd_open', side_effect=open_pidfd, create=True), \
             patch('virtual_display.signal.pidfd_send_signal', side_effect=bound_signal, create=True), \
             patch('virtual_display.os.close', side_effect=closed.append), \
             patch.object(virtual_display, 'select', select_module, create=True):
            virtual_display.stop_attached(address)
        self.assertIn(signal.SIGTERM, owned_signals, 'the original private-bus process must receive cleanup')
        self.assertEqual(replacement_signals, [], 'a reused PID must never receive a cleanup signal')
        self.assertEqual(closed, list(descriptors), 'every bound process descriptor is closed')

    def test_replacement_during_pidfd_open_is_never_signalled(self):
        entry = Mock(name='owned-process-entry')
        entry.name = '424242'
        proc = Mock()
        proc.iterdir.return_value = [entry]
        with patch('virtual_display.Path', return_value=proc), \
             patch('virtual_display._attached_start_ticks', side_effect=[100, 200]), \
             patch('virtual_display.os.pidfd_open', return_value=91002) as opened, \
             patch('virtual_display.signal.pidfd_send_signal') as sent, \
             patch('virtual_display.os.kill') as bare_signal, \
             patch('virtual_display.os.close') as closed:
            virtual_display.stop_attached('unix:path=/private-fixture/bus')
        opened.assert_called_once_with(424242)
        sent.assert_not_called()
        bare_signal.assert_not_called()
        closed.assert_called_once_with(91002)

    def test_exited_process_during_pidfd_open_is_ignored(self):
        entry = Mock(name='exited-process-entry')
        entry.name = '424242'
        proc = Mock()
        proc.iterdir.return_value = [entry]
        with patch('virtual_display.Path', return_value=proc), \
             patch('virtual_display._attached_start_ticks', return_value=100), \
             patch('virtual_display.os.pidfd_open', side_effect=ProcessLookupError), \
             patch('virtual_display.signal.pidfd_send_signal') as sent, \
             patch('virtual_display.os.kill') as bare_signal, \
             patch('virtual_display.os.close') as closed:
            virtual_display.stop_attached('unix:path=/private-fixture/bus')
        sent.assert_not_called()
        bare_signal.assert_not_called()
        closed.assert_not_called()

    def test_attached_cleanup_accepts_high_process_descriptors(self):
        read_end, write_end = os.pipe()
        try:
            descriptor = fcntl.fcntl(read_end, fcntl.F_DUPFD_CLOEXEC, 2048)
        finally:
            os.close(read_end)
        owned = descriptor
        real_close = os.close
        entry = Mock(name='high-descriptor-process')
        entry.name = '424242'
        proc = Mock()
        proc.iterdir.return_value = [entry]

        def send(bound, value):
            self.assertEqual(bound, descriptor)
            self.assertEqual(value, signal.SIGTERM)
            os.write(write_end, b'exited')

        def close(bound):
            nonlocal owned
            self.assertEqual(bound, owned)
            real_close(bound)
            owned = None

        try:
            with patch('virtual_display.Path', return_value=proc), \
                 patch('virtual_display._attached_start_ticks', return_value=100), \
                 patch('virtual_display.os.pidfd_open', return_value=descriptor), \
                 patch('virtual_display.signal.pidfd_send_signal', side_effect=send) as sent, \
                 patch('virtual_display.os.kill') as bare_signal, \
                 patch('virtual_display.os.close', side_effect=close):
                virtual_display.stop_attached('unix:path=/private-fixture/bus')
            sent.assert_called_once_with(descriptor, signal.SIGTERM)
            bare_signal.assert_not_called()
            self.assertIsNone(owned, 'cleanup closes the high descriptor after observing exit')
        finally:
            if owned is not None:
                real_close(owned)
            real_close(write_end)

    def test_missing_pidfd_support_refuses_availability(self):
        with patch('virtual_display.os.pidfd_open', None), \
             patch('virtual_display.subprocess.Popen') as spawn:
            self.assertEqual(virtual_display.available(), 'pidfd process ownership is unavailable')
        spawn.assert_not_called()

    def test_blocked_pidfd_support_closes_the_probe_descriptor(self):
        with patch('virtual_display.os.pidfd_open', return_value=91003), \
             patch('virtual_display.signal.pidfd_send_signal', side_effect=PermissionError), \
             patch('virtual_display.os.close') as closed:
            self.assertEqual(virtual_display.available(), 'pidfd process ownership is unavailable')
        closed.assert_called_once_with(91003)

    def test_direct_dispatch_refuses_unavailable_pidfd_before_starting(self):
        with patch('virtual_display.os.pidfd_open', None), \
             patch('virtual_display._run_inside') as dispatch:
            with self.assertRaisesRegex(RuntimeError, 'pidfd process ownership is unavailable'):
                virtual_display.run_inside(['fixture-client'], 1280, 800)
        dispatch.assert_not_called()

    def test_probe_timeout_can_recover_inside_the_original_deadline(self):
        process = Mock()
        process.poll.return_value = None
        with patch('virtual_display.time.monotonic', side_effect=[0, 0, 2]), \
             patch('virtual_display.time.sleep'), \
             patch('virtual_display.subprocess.run', side_effect=[
                 subprocess.TimeoutExpired('xdpyinfo', 2), Mock(returncode=0),
             ]) as probe:
            self.assertTrue(virtual_display.wait_for_display(process, {'DISPLAY': ':1234'}, lambda: False))
        self.assertEqual(probe.call_count, 2)
        self.assertEqual(probe.call_args.kwargs['env'], {'DISPLAY': ':1234'})

    def test_probe_timeout_never_establishes_readiness_after_the_deadline(self):
        process = Mock()
        process.poll.return_value = None
        with patch('virtual_display.time.monotonic', side_effect=[0, 0, 11]), \
             patch('virtual_display.time.sleep'), \
             patch('virtual_display.subprocess.run', side_effect=subprocess.TimeoutExpired('xdpyinfo', 2)) as probe:
            self.assertFalse(virtual_display.wait_for_display(process, {}, lambda: False))
        self.assertEqual(probe.call_count, 1)

    def test_cancelled_startup_does_not_probe(self):
        process = Mock()
        process.poll.return_value = None
        with patch('virtual_display.subprocess.run') as probe:
            self.assertFalse(virtual_display.wait_for_display(process, {}, lambda: True))
        probe.assert_not_called()

    def test_sigterm_cleans_the_owned_display_and_runtime(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        root = Path(__file__).resolve().parents[2]
        script = """import json, os, time
print('OWNED ' + json.dumps({key: os.environ[key] for key in ('DISPLAY', 'XDG_RUNTIME_DIR', 'OPENCHAMBER_PARITY_XVFB_PID')}), flush=True)
time.sleep(120)
"""
        process = subprocess.Popen([sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
                                   cwd=root, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            output = b""
            deadline = time.monotonic() + 30
            while b'OWNED ' not in output or not output.split(b'OWNED ', 1)[1].endswith(b'\n'):
                remaining = max(0, deadline - time.monotonic())
                self.assertTrue(select.select([process.stdout], [], [], remaining)[0], "private session did not start")
                chunk = os.read(process.stdout.fileno(), 4096)
                self.assertTrue(chunk, output.decode())
                output += chunk
            line = next(line for line in output.decode().splitlines() if line.startswith('OWNED '))
            record = json.loads(line[6:])
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(timeout=40), 128 + signal.SIGTERM)
            self.assertFalse(Path(record['XDG_RUNTIME_DIR']).exists())
            self.assertFalse(Path('/proc', record['OPENCHAMBER_PARITY_XVFB_PID']).exists())
            self.assertFalse(Path('/tmp/.X11-unix', 'X' + record['DISPLAY'][1:]).exists())
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=40)
            process.stdout.close()

    def test_unowned_display_is_refused(self):
        with patch.dict(os.environ, {virtual_display.XVFB_PID: "0"}):
            with self.assertRaisesRegex(RuntimeError, "does not own"):
                virtual_display.verify_owned_display()

    def test_failure_still_cleans_the_owned_display_and_runtime(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        script = """import json, os, sys
sys.path.insert(0, 'scripts/parity')
import virtual_display
virtual_display.verify_owned_display()
assert not any(key in os.environ for key in virtual_display.INPUT_SESSION_VARIABLES)
print('OWNED ' + json.dumps({key: os.environ[key] for key in ('DISPLAY', 'XDG_RUNTIME_DIR', 'OPENCHAMBER_PARITY_XVFB_PID')}), flush=True)
sys.exit(7)
"""
        root = Path(__file__).resolve().parents[2]
        env = dict(os.environ, IBUS_ADDRESS="unix:path=/not-the-private-bus", XMODIFIERS="@im=personal")
        result = subprocess.run([sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
                                cwd=root, env=env, capture_output=True, text=True, timeout=45)
        self.assertEqual(result.returncode, 7, result.stderr)
        record = json.loads(next(line[6:] for line in result.stdout.splitlines() if line.startswith('OWNED ')))
        self.assertFalse(Path(record['XDG_RUNTIME_DIR']).exists())
        self.assertFalse(Path('/proc', record['OPENCHAMBER_PARITY_XVFB_PID']).exists())
        self.assertFalse(Path('/tmp/.X11-unix', 'X' + record['DISPLAY'][1:]).exists())


if __name__ == '__main__':
    unittest.main()
