"""Pure mocked portal ownership/isolation tests. These do not qualify a chooser."""

import errno
import itertools
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

import portal_fixture
from native_interactions import RunInterrupted
import virtual_display


class PortalFixtureTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(dir='/tmp/opencode')
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name)
        self.runtime = self.base / 'runtime'
        self.runtime.mkdir(mode=0o700)
        (self.runtime / 'services').mkdir()
        (self.runtime / 'bus.conf').write_text(virtual_display.BUS_CONFIG.format(
            work=self.runtime, services=self.runtime / 'services'))
        self.address = f'unix:path={self.runtime}/bus-socket,guid=synthetic'
        (self.runtime / 'bus').write_text(self.address)
        self.environment = {
            'PATH': '/usr/bin:/bin', 'DISPLAY': ':4242', 'XAUTHORITY': str(self.runtime / 'Xauthority'),
            'XDG_RUNTIME_DIR': str(self.runtime), 'DBUS_SESSION_BUS_ADDRESS': self.address,
            virtual_display.INSIDE: '1', virtual_display.XVFB_PID: '41',
        }
        self.source = {**self.environment, 'HOME': '/ambient-home', 'XDG_DATA_HOME': '/ambient-data',
                       'WAYLAND_DISPLAY': 'wayland-0', 'WAYLAND_SOCKET': '7', 'XDG_SESSION_TYPE': 'wayland',
                       'DBUS_STARTER_ADDRESS': 'ambient-bus', 'DBUS_STARTER_BUS_TYPE': 'session',
                       'DBUS_SYSTEM_BUS_ADDRESS': 'ambient-system-bus', 'PROVIDER_API_KEY': 'synthetic',
                       'GTK_MODULES': 'ambient-module', 'XDG_CURRENT_DESKTOP': 'KDE'}
        self.interrupts = self.enterContext(portal_fixture.Interrupts())
        self.fixture = portal_fixture.PortalFixture(self.base / 'portal', self.source, self.interrupts)
        self.names = {}
        self.processes = []
        self.identities = {}
        self.patch(patch.dict(os.environ, self.environment, clear=True))
        self.verify = self.patch(patch.object(virtual_display, 'verify_owned_display'))
        self.patch(patch.object(Path, 'is_file', return_value=True))
        self.patch(patch.object(os, 'access', return_value=True))
        self.patch(patch.object(portal_fixture, 'process_identity', side_effect=lambda pid: self.identities.get(pid)))
        self.pidfd_open = self.patch(patch.object(os, 'pidfd_open', side_effect=AssertionError('non-atomic acquisition')))
        self.patch(patch.object(os, 'waitid', return_value=None))
        self.close_fd = self.patch(patch.object(os, 'close'))
        self.run = self.patch(patch.object(subprocess, 'run', side_effect=self.bus))
        self.popen = self.patch(patch.object(subprocess, 'Popen', side_effect=AssertionError('non-atomic launch')))
        self.spawner = self.patch(patch.object(portal_fixture, '_PidfdSpawner')).return_value
        self.spawn = self.spawner.spawn
        self.spawn.side_effect = self.launch
        self.spawner.pid.side_effect = lambda process: process.fd.value - 10000
        self.send_signal = self.patch(patch.object(signal, 'pidfd_send_signal', side_effect=self.signal))
        self.patch(patch.object(portal_fixture.time, 'sleep'))

    def patch(self, patcher):
        value = patcher.start()
        self.addCleanup(patcher.stop)
        return value

    def launch(self, process, executable, env, cwd, log):
        pid = 1001 + len(self.processes)
        process.fd.value = pid + 10000
        self.processes.append(process)
        self.identities[pid] = str(100000 + pid)
        name = portal_fixture.BACKEND_NAME if executable == portal_fixture.BACKEND else portal_fixture.FRONTEND_NAME
        self.names[name] = pid

    def signal(self, pidfd, sig):
        process = next(item for item in self.processes if item.fd.value == pidfd)
        process.returncode = -sig

    def bus(self, argv, **kwargs):
        method, name = argv[-3], argv[-1]
        if method == 'NameHasOwner':
            value = 'b true' if name in self.names else 'b false'
        elif method == 'GetNameOwner':
            value = f's ":1.{self.names[name]}"'
        elif method == 'GetConnectionUnixProcessID':
            value = 'u ' + name.rsplit('.', 1)[-1]
        else:
            raise AssertionError(method)
        return subprocess.CompletedProcess(argv, 0, stdout=value, stderr='')

    def test_ready_receipt_contains_exact_identities_and_no_workflow_claim(self):
        receipt = self.fixture.start()
        self.assertEqual(receipt['backend'], {'pid': 1001, 'starttime': '101001'})
        self.assertEqual(receipt['frontend'], {'pid': 1002, 'starttime': '101002'})
        self.assertIn('picker workflow unqualified', receipt['scope'])
        self.assertEqual([call.args[1] for call in self.spawn.call_args_list],
                         [portal_fixture.BACKEND, portal_fixture.FRONTEND])
        self.assertGreaterEqual(self.verify.call_count, self.spawn.call_count + self.run.call_count)
        self.popen.assert_not_called()
        self.pidfd_open.assert_not_called()
        cleanup = self.fixture.close()
        self.assertEqual(cleanup['remainingPids'], [])
        self.assertEqual(cleanup['errors'], [])

    def test_private_paths_and_environment_scrub(self):
        self.fixture.start()
        env = self.spawn.call_args.args[2]
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'TMPDIR'):
            path = Path(env[key])
            self.assertTrue(path.is_relative_to(self.fixture.root))
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(self.fixture.root.stat().st_mode), 0o700)
        for key in self.source.keys() - self.environment.keys() - {'HOME', 'XDG_DATA_HOME', 'DBUS_SYSTEM_BUS_ADDRESS'}:
            self.assertNotIn(key, env)
        self.assertEqual(env['GDK_BACKEND'], 'x11')
        self.assertEqual(env['DBUS_SESSION_BUS_ADDRESS'], self.address)
        self.assertNotEqual(env['DBUS_SYSTEM_BUS_ADDRESS'], self.source['DBUS_SYSTEM_BUS_ADDRESS'])
        self.assertEqual((self.fixture.root / 'config/xdg-desktop-portal/portals.conf').read_text(),
                         portal_fixture.PORTALS_CONFIG)
        self.fixture.close()

    def test_existing_root_is_preserved(self):
        self.fixture.root.mkdir()
        marker = self.fixture.root / 'existing.txt'
        marker.write_text('preserve')
        with self.assertRaises(FileExistsError):
            self.fixture.start()
        self.assertEqual(marker.read_text(), 'preserve')
        self.assertEqual(list(self.fixture.root.iterdir()), [marker])
        self.spawn.assert_not_called()

    def test_nonempty_activation_directory_is_rejected(self):
        (self.runtime / 'services/ambient.service').write_text('not allowed')
        with self.assertRaisesRegex(RuntimeError, 'empty D-Bus service directory'):
            self.fixture.start()
        self.spawn.assert_not_called()
        self.assertFalse(self.fixture.root.exists())

    def test_changed_bus_configuration_is_rejected(self):
        config = self.runtime / 'bus.conf'
        config.write_text(config.read_text().replace('<keep_umask/>', '<standard_session_servicedirs/>'))
        with self.assertRaisesRegex(RuntimeError, 'configuration differs'):
            self.fixture.start()
        self.spawn.assert_not_called()

    def test_unowned_display_is_rejected_before_creating_profile(self):
        self.verify.side_effect = RuntimeError('not an owned display')
        with self.assertRaisesRegex(RuntimeError, 'not an owned display'):
            self.fixture.start()
        self.assertFalse(self.fixture.root.exists())
        self.spawn.assert_not_called()

    def test_existing_portal_owner_is_not_replaced_or_signalled(self):
        self.names[portal_fixture.FRONTEND_NAME] = 999
        with self.assertRaisesRegex(RuntimeError, 'already owned'):
            self.fixture.start()
        self.spawn.assert_not_called()
        self.send_signal.assert_not_called()

    def test_bus_owner_must_be_the_started_process(self):
        def wrong_owner(*args):
            self.launch(*args)
            self.names[portal_fixture.BACKEND_NAME] = 999
        self.spawn.side_effect = wrong_owner
        with self.assertRaisesRegex(RuntimeError, 'other than the started backend'):
            self.fixture.start()
        self.assertEqual(self.spawn.call_count, 1)
        self.assertEqual(self.fixture.close()['remainingPids'], [])

    def test_partial_start_failure_stops_backend_and_retains_artifacts(self):
        def fail_frontend(process, executable, *args):
            if executable == portal_fixture.FRONTEND:
                raise PermissionError('synthetic frontend launch denied')
            self.launch(process, executable, *args)
        self.spawn.side_effect = fail_frontend
        with self.assertRaisesRegex(PermissionError, 'frontend launch denied'):
            self.fixture.start()
        self.assertEqual(self.fixture.close()['remainingPids'], [])
        self.assertIn('frontend launch denied', (self.fixture.root / 'startup-failed.txt').read_text())
        for name in ('backend.log', 'frontend.log', 'cleanup.json', 'config/xdg-desktop-portal/portals.conf'):
            self.assertTrue((self.fixture.root / name).exists())
        self.assertFalse((self.fixture.root / 'startup.json').exists())

    def test_missing_dependency_fails_closed_without_starting_extra_helpers(self):
        def dependency_error(process, executable, env, cwd, log):
            self.launch(process, executable, env, cwd, log)
            if executable == portal_fixture.FRONTEND:
                log.write(b'Failed to create document portal proxy: org.freedesktop.portal.Documents not provided\n')
        self.spawn.side_effect = dependency_error
        with self.assertRaisesRegex(RuntimeError, 'requires unavailable org.freedesktop.portal.Documents'):
            self.fixture.start()
        self.assertEqual(self.spawn.call_count, 2)
        self.assertEqual(self.fixture.close()['remainingPids'], [])

    def test_deadline_does_not_turn_missing_owner_into_readiness(self):
        def no_name(*args):
            self.launch(*args)
            self.names.clear()
        self.spawn.side_effect = no_name
        self.patch(patch.object(portal_fixture.time, 'monotonic', side_effect=itertools.count(0, 0.1).__next__))
        self.patch(patch.object(portal_fixture, 'READINESS_TIMEOUT', 0.5))
        with self.assertRaisesRegex(TimeoutError, 'did not acquire'):
            self.fixture.start()
        self.assertFalse((self.fixture.root / 'startup.json').exists())
        self.assertEqual(self.fixture.close()['remainingPids'], [])

    def test_observation_failure_is_not_treated_as_an_unowned_bus_name(self):
        self.run.return_value = subprocess.CompletedProcess([], 1, stdout='', stderr='synthetic failure')
        self.run.side_effect = None
        with self.assertRaisesRegex(RuntimeError, 'D-Bus observation failed'):
            self.fixture.start()
        self.spawn.assert_not_called()

    def test_identity_mismatch_refuses_signals_and_is_reported(self):
        self.fixture.start()
        self.identities[1002] = 'reused'
        receipt = self.fixture.close()
        self.assertIn(1002, receipt['remainingPids'])
        self.assertTrue(any('identity changed' in error for error in receipt['errors']))
        self.assertEqual([call.args[0] for call in self.send_signal.call_args_list], [11001])
        self.assertNotIn(11002, [call.args[0] for call in self.close_fd.call_args_list])
        self.assertEqual(json.loads((self.fixture.root / 'cleanup.json').read_text()), receipt)

    def test_pidfd_acquisition_failure_creates_no_uncontrolled_child(self):
        error = OSError(errno.EMFILE, 'synthetic pidfd allocation failure')
        self.spawn.side_effect = error
        with self.assertRaises(OSError) as caught:
            self.fixture.start()
        self.assertIs(caught.exception, error)
        self.assertEqual(self.fixture.close(), {'remainingPids': [], 'errors': [], 'processes': []})
        self.assertEqual(self.processes, [])
        self.send_signal.assert_not_called()
        self.popen.assert_not_called()
        self.pidfd_open.assert_not_called()
        self.assertIn('synthetic pidfd allocation failure', (self.fixture.root / 'startup-failed.txt').read_text())

    def test_frontend_pidfd_failure_stops_only_the_owned_backend(self):
        error = OSError(errno.ENOSYS, 'synthetic atomic pidfd creation unavailable')

        def fail_frontend(process, executable, *args):
            if executable == portal_fixture.FRONTEND:
                raise error
            self.launch(process, executable, *args)

        self.spawn.side_effect = fail_frontend
        with self.assertRaises(OSError) as caught:
            self.fixture.start()
        self.assertIs(caught.exception, error)
        cleanup = self.fixture.close()
        self.assertEqual(cleanup['remainingPids'], [])
        self.assertEqual(cleanup['processes'], [{'pid': 1001, 'starttime': '101001', 'exitCode': -signal.SIGTERM}])
        self.send_signal.assert_called_once_with(11001, signal.SIGTERM)

    def test_interrupted_atomic_creation_keeps_handle_before_pid_recording(self):
        def interrupt(*args):
            self.launch(*args)
            raise KeyboardInterrupt('synthetic interruption after clone3')

        self.spawn.side_effect = interrupt
        with self.assertRaises(KeyboardInterrupt):
            self.fixture.start()
        self.send_signal.assert_called_once_with(11001, signal.SIGTERM)
        cleanup = self.fixture.close()
        self.assertEqual(cleanup['remainingPids'], [])
        self.assertEqual(cleanup['processes'], [{'pid': None, 'starttime': None, 'exitCode': -signal.SIGTERM}])
        self.spawner.pid.assert_not_called()

    def test_pid_recording_failure_still_stops_the_atomic_child(self):
        error = OSError(errno.EIO, 'synthetic pidfd PID observation failure')
        self.spawner.pid.side_effect = error
        with self.assertRaises(OSError) as caught:
            self.fixture.start()
        self.assertIs(caught.exception, error)
        self.send_signal.assert_called_once_with(11001, signal.SIGTERM)
        self.assertEqual(self.fixture.close()['remainingPids'], [])

    def test_missing_atomic_support_starts_nothing_and_preserves_the_blocker(self):
        error = RuntimeError('synthetic atomic creation unavailable')
        portal_fixture._PidfdSpawner.side_effect = error
        with self.assertRaises(RuntimeError) as caught:
            self.fixture.start()
        self.assertIs(caught.exception, error)
        self.assertFalse(self.fixture.root.exists())
        self.spawn.assert_not_called()
        self.popen.assert_not_called()
        self.send_signal.assert_not_called()

    def test_failed_shutdown_retains_exact_handle_for_retry(self):
        self.fixture.start()
        self.send_signal.side_effect = PermissionError('synthetic signal denied')
        first = self.fixture.close()
        self.assertEqual(first['remainingPids'], [1001, 1002])
        self.close_fd.assert_not_called()
        self.send_signal.side_effect = self.signal
        second = self.fixture.close()
        self.assertEqual(second['remainingPids'], [])
        self.assertEqual(second['errors'], [])
        self.assertEqual([call.args[0] for call in self.close_fd.call_args_list], [11002, 11001])

    def test_cleanup_receipt_failure_preserves_original_startup_error(self):
        error = OSError(errno.EMFILE, 'synthetic original pidfd failure')
        self.spawn.side_effect = error
        original = Path.write_text

        def fail_cleanup(path, *args, **kwargs):
            if path.name == 'cleanup.json':
                raise OSError('synthetic cleanup receipt failure')
            return original(path, *args, **kwargs)

        self.patch(patch.object(Path, 'write_text', fail_cleanup))
        with self.assertRaises(OSError) as caught:
            self.fixture.start()
        self.assertIs(caught.exception, error)
        self.assertTrue(any('synthetic cleanup receipt failure' in note for note in error.__notes__))
        self.assertIn('synthetic original pidfd failure', (self.fixture.root / 'startup-failed.txt').read_text())

    def test_close_is_idempotent_and_rejects_restart(self):
        self.fixture.start()
        first = self.fixture.close()
        calls = self.send_signal.call_count
        self.assertEqual(self.fixture.close(), first)
        self.assertEqual(self.send_signal.call_count, calls)
        with self.assertRaisesRegex(RuntimeError, 'only once'):
            self.fixture.start()

    def test_interrupted_handle_close_commits_once_and_keeps_other_custody(self):
        self.fixture.start()
        handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        frontend = self.processes[1]
        closed = []

        def close_then_interrupt(descriptor):
            closed.append(descriptor)
            if descriptor == frontend.fd.value:
                self.assertFalse(frontend.closed)
                self.interrupts.receive(signal.SIGINT, None)

        self.close_fd.side_effect = close_then_interrupt
        with self.assertRaises(RunInterrupted):
            self.fixture.close()
        self.assertTrue(frontend.closed)
        self.assertFalse(self.processes[0].closed)
        self.assertIsNone(self.processes[0].returncode)
        self.assertEqual(closed, [11002])
        self.assertIsNone(self.fixture._cleanup)
        receipt = self.fixture.close()
        self.assertEqual(receipt['remainingPids'], [])
        self.assertEqual(receipt['errors'], [])
        self.assertEqual(closed, [11002, 11001])
        for sig, handler in handlers.items():
            self.assertEqual(signal.getsignal(sig), handler)

    def test_inactive_interrupt_scope_retains_unclosed_handles_for_retry(self):
        self.fixture.start()
        for process in self.processes:
            process.returncode = 0
        with patch.object(signal, 'getsignal', return_value=signal.default_int_handler):
            receipt = self.fixture.close()
        self.assertEqual(receipt['remainingPids'], [])
        self.assertTrue(all('active main-thread Interrupts' in error for error in receipt['errors']))
        self.close_fd.assert_not_called()
        self.assertFalse(any(process.closed for process in self.processes))
        self.assertEqual(self.fixture.close()['errors'], [])
        self.assertEqual(self.close_fd.call_count, 2)


class AtomicSpawnTests(unittest.TestCase):
    def setUp(self):
        self.interrupts = self.enterContext(portal_fixture.Interrupts())
        self.libc = Mock()
        for name in ('pidfd_spawn', 'posix_spawnattr_init', 'posix_spawnattr_destroy',
                     'posix_spawnattr_setflags', 'posix_spawnattr_setsigdefault',
                     'sigemptyset', 'sigaddset', 'posix_spawn_file_actions_init',
                     'posix_spawn_file_actions_destroy', 'posix_spawn_file_actions_addopen',
                     'posix_spawn_file_actions_adddup2', 'posix_spawn_file_actions_addchdir_np',
                     'posix_spawn_file_actions_addclosefrom_np'):
            getattr(self.libc, name).return_value = 0
        library = patch.object(portal_fixture.ctypes, 'CDLL', return_value=self.libc)
        library.start()
        self.addCleanup(library.stop)
        self.spawner = portal_fixture._PidfdSpawner()

    def test_pidfd_allocation_error_never_falls_back_to_popen_or_pidfd_open(self):
        self.libc.pidfd_spawn.return_value = errno.EMFILE
        process = portal_fixture._PortalProcess(self.interrupts)
        with patch.object(subprocess, 'Popen') as popen, patch.object(os, 'pidfd_open') as open_pidfd:
            with self.assertRaises(OSError) as caught:
                self.spawner.spawn(process, portal_fixture.BACKEND, {}, Path('/private-root'), Mock(fileno=lambda: 9))
        self.assertEqual(caught.exception.errno, errno.EMFILE)
        self.assertFalse(process.started)
        self.assertEqual(process.fd.value, -1)
        popen.assert_not_called()
        open_pidfd.assert_not_called()

    def test_atomic_result_is_written_into_the_registered_process(self):
        process = portal_fixture._PortalProcess(self.interrupts)

        def create(output, *_):
            output._obj.value = 11001
            return 0

        self.libc.pidfd_spawn.side_effect = create
        self.spawner.spawn(process, portal_fixture.BACKEND, {'LANG': 'C'}, Path('/private-root'), Mock(fileno=lambda: 9))
        self.assertTrue(process.started)
        self.assertEqual(process.fd.value, 11001)
        self.assertIsNone(process.pid)
        self.libc.posix_spawnattr_setflags.assert_called_once()
        self.assertEqual(self.libc.posix_spawnattr_setflags.call_args.args[1], 0x80 | 0x04)
        self.assertEqual(self.libc.posix_spawn_file_actions_addclosefrom_np.call_args.args[1], 3)

    def test_missing_atomic_api_is_a_blocker(self):
        del self.libc.pidfd_spawn
        with self.assertRaisesRegex(RuntimeError, 'refusing non-atomic child creation'):
            portal_fixture._PidfdSpawner()

    def test_log_duplication_precedes_stdin_replacement_for_low_descriptors(self):
        for log_fd in (0, 1, 2, 9):
            with self.subTest(log_fd=log_fd):
                descriptors = {0: 'stdin', 1: 'stdout', 2: 'stderr', log_fd: 'portal log'}

                def duplicate(_actions, source, target):
                    descriptors[target] = descriptors[source]
                    return 0

                def open_stdin(_actions, descriptor, path, _flags, _mode):
                    descriptors[descriptor] = path
                    return 0

                self.libc.posix_spawn_file_actions_adddup2.side_effect = duplicate
                self.libc.posix_spawn_file_actions_addopen.side_effect = open_stdin
                process = portal_fixture._PortalProcess(self.interrupts)
                self.spawner.spawn(process, portal_fixture.BACKEND, {}, Path('/private-root'),
                                   Mock(fileno=lambda: log_fd))
                self.assertEqual([descriptors[fd] for fd in (0, 1, 2)],
                                 [b'/dev/null', 'portal log', 'portal log'])


class PortalProcessTests(unittest.TestCase):
    def setUp(self):
        self.interrupts = self.enterContext(portal_fixture.Interrupts())

    def test_waits_and_reaps_through_the_exact_pidfd(self):
        process = portal_fixture._PortalProcess(self.interrupts)
        process.fd.value = 11001
        result = Mock(si_code=os.CLD_EXITED, si_status=7)
        with patch.object(os, 'waitid', side_effect=[None, result]) as waitid, \
             patch.object(portal_fixture.select, 'poll') as poller:
            self.assertEqual(process.wait(timeout=1), 7)
            self.assertEqual(process.poll(), 7)
        self.assertEqual(waitid.call_count, 2)
        for call in waitid.call_args_list:
            self.assertEqual(call.args, (os.P_PIDFD, 11001, os.WEXITED | os.WNOHANG))
        poller.return_value.register.assert_called_once_with(11001, portal_fixture.select.POLLIN)
        poller.return_value.poll.assert_called_once()

    def test_signalled_exit_has_popen_compatible_returncode(self):
        process = portal_fixture._PortalProcess(self.interrupts)
        process.fd.value = 11001
        with patch.object(os, 'waitid', return_value=Mock(si_code=os.CLD_KILLED, si_status=signal.SIGTERM)):
            self.assertEqual(process.poll(), -signal.SIGTERM)

    def test_wait_timeout_keeps_the_atomic_handle(self):
        process = portal_fixture._PortalProcess(self.interrupts)
        process.fd.value = 11001
        with patch.object(os, 'waitid', return_value=None), \
             patch.object(portal_fixture.time, 'monotonic', side_effect=[0, 2]), \
             patch.object(os, 'close') as close:
            with self.assertRaises(subprocess.TimeoutExpired):
                process.wait(timeout=1)
        self.assertEqual(process.fd.value, 11001)
        self.assertIsNone(process.returncode)
        close.assert_not_called()

    def test_pending_signal_cannot_lose_a_consumed_exit_record(self):
        for sig in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=sig), portal_fixture.Interrupts() as interrupts:
                process = portal_fixture._PortalProcess(interrupts)
                process.fd.value = 11001

                class ExitRecord:
                    si_code = os.CLD_EXITED

                    @property
                    def si_status(self):
                        interrupts.receive(sig, None)
                        return 7

                with patch.object(os, 'waitid', side_effect=[ExitRecord(), ChildProcessError('already consumed')]) as waitid:
                    with self.assertRaises(RunInterrupted):
                        process.poll()
                    self.assertEqual(process.returncode, 7)
                    self.assertEqual(process.poll(), 7)
                    waitid.assert_called_once_with(os.P_PIDFD, 11001, os.WEXITED | os.WNOHANG)
                self.assertTrue(interrupts.delivered)
                self.assertEqual(signal.getsignal(sig), interrupts.receive)

    def test_pending_signal_after_waitid_returns_still_publishes_status(self):
        process = portal_fixture._PortalProcess(self.interrupts)
        process.fd.value = 11001

        def consumed(*_):
            self.interrupts.receive(signal.SIGINT, None)
            return Mock(si_code=os.CLD_KILLED, si_status=signal.SIGTERM)

        with patch.object(os, 'waitid', side_effect=consumed) as waitid:
            with self.assertRaises(RunInterrupted):
                process.poll()
            self.assertEqual(process.poll(), -signal.SIGTERM)
            self.assertEqual(waitid.call_count, 1)

    def test_high_numbered_handle_uses_descriptor_independent_poll(self):
        process = portal_fixture._PortalProcess(self.interrupts)
        process.fd.value = 11001
        result = Mock(si_code=os.CLD_EXITED, si_status=0)
        with patch.object(os, 'waitid', side_effect=[None, result]), \
             patch.object(portal_fixture.time, 'monotonic', side_effect=[0, 0]), \
             patch.object(portal_fixture.select, 'select', side_effect=ValueError('FD_SETSIZE exceeded')) as limited, \
             patch.object(portal_fixture.select, 'poll') as poller:
            self.assertEqual(process.wait(timeout=1), 0)
        limited.assert_not_called()
        poller.return_value.register.assert_called_once_with(11001, portal_fixture.select.POLLIN)
        poller.return_value.poll.assert_called_once_with(1000)


if __name__ == '__main__':
    unittest.main()
