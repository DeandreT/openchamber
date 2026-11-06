"""Own only the GTK file-chooser portal on the parity wrapper's private bus.

PortalFixture.start() proves service ownership, not a displayed or usable picker.
The caller owns native input, chooser observations and screenshots. All profiles,
logs and failure receipts remain under the new root after close(). No activation
files or additional services are installed, started or removed.
"""

from dataclasses import dataclass
import ctypes
import json
import os
from pathlib import Path
import select
import shlex
import signal
import subprocess
import threading
import time
import traceback

from native_interactions import Interrupts, process_identity
import virtual_display


BACKEND = Path('/usr/lib/xdg-desktop-portal-gtk')
FRONTEND = Path('/usr/lib/xdg-desktop-portal')
BUSCTL = Path('/usr/bin/busctl')
BACKEND_NAME = 'org.freedesktop.impl.portal.desktop.gtk'
FRONTEND_NAME = 'org.freedesktop.portal.Desktop'
UNAPPROVED_HELPERS = ('org.freedesktop.impl.portal.PermissionStore',
                      'org.freedesktop.portal.Documents')
PORTALS_CONFIG = ('[preferred]\ndefault=none\n'
                  'org.freedesktop.impl.portal.FileChooser=gtk\n')
READINESS_TIMEOUT = 15.0
STOP_TIMEOUT = 5.0


def _require_interrupts(interrupts):
    if (not isinstance(interrupts, Interrupts)
            or threading.current_thread() is not threading.main_thread()
            or any(signal.getsignal(sig) != interrupts.receive for sig in (signal.SIGINT, signal.SIGTERM))):
        raise RuntimeError('portal custody requires its active main-thread Interrupts scope')


class _Sigset(ctypes.Structure):
    _fields_ = [('bits', ctypes.c_ulong * (128 // ctypes.sizeof(ctypes.c_ulong)))]


class _SpawnAttributes(ctypes.Structure):
    # Linux glibc's public spawn.h ABI. The reserved tail includes __cgroup.
    _fields_ = [('flags', ctypes.c_short), ('pgrp', ctypes.c_int),
                ('sigdefault', _Sigset), ('sigmask', _Sigset),
                ('priority', ctypes.c_int), ('policy', ctypes.c_int),
                ('reserved', ctypes.c_int * 16)]


class _SpawnActions(ctypes.Structure):
    _fields_ = [('allocated', ctypes.c_int), ('used', ctypes.c_int),
                ('actions', ctypes.c_void_p), ('reserved', ctypes.c_int * 16)]


class _PortalProcess:
    def __init__(self, interrupts):
        # Register this storage before entering libc: signal delivery can interrupt
        # Python immediately after clone3 has filled the atomic pidfd result.
        self.fd = ctypes.c_int(-1)
        self.pid = None
        self.returncode = None
        self.closed = False
        self.interrupts = interrupts

    @property
    def started(self):
        return self.fd.value >= 0

    def poll(self):
        if self.started and self.returncode is None:
            _require_interrupts(self.interrupts)
            # waitid consumes the only exit record. Publish it before the active
            # driver delivers a queued SIGINT/SIGTERM, including one from another
            # thread. No handler or thread-local signal mask changes here.
            with self.interrupts.defer():
                result = os.waitid(os.P_PIDFD, self.fd.value, os.WEXITED | os.WNOHANG)
                if result is not None:
                    self.returncode = result.si_status if result.si_code == os.CLD_EXITED else -result.si_status
        return self.returncode

    def wait(self, timeout):
        deadline = time.monotonic() + timeout
        ready = select.poll()
        ready.register(self.fd.value, select.POLLIN)
        while self.poll() is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(self.pid, timeout)
            ready.poll(max(1, int(remaining * 1000)))
        return self.returncode


class _PidfdSpawner:
    """Create the child and its custody handle together, without a PID fallback."""

    def __init__(self):
        self.libc = ctypes.CDLL(None, use_errno=True)
        pointer = ctypes.POINTER
        strings = pointer(ctypes.c_char_p)
        signatures = {
            'pidfd_spawn': [pointer(ctypes.c_int), ctypes.c_char_p, pointer(_SpawnActions),
                            pointer(_SpawnAttributes), strings, strings],
            'pidfd_getpid': [ctypes.c_int],
            'posix_spawnattr_init': [pointer(_SpawnAttributes)],
            'posix_spawnattr_destroy': [pointer(_SpawnAttributes)],
            'posix_spawnattr_setflags': [pointer(_SpawnAttributes), ctypes.c_short],
            'posix_spawnattr_setsigdefault': [pointer(_SpawnAttributes), pointer(_Sigset)],
            'sigemptyset': [pointer(_Sigset)],
            'sigaddset': [pointer(_Sigset), ctypes.c_int],
            'posix_spawn_file_actions_init': [pointer(_SpawnActions)],
            'posix_spawn_file_actions_destroy': [pointer(_SpawnActions)],
            'posix_spawn_file_actions_addopen': [pointer(_SpawnActions), ctypes.c_int,
                                                ctypes.c_char_p, ctypes.c_int, ctypes.c_uint],
            'posix_spawn_file_actions_adddup2': [pointer(_SpawnActions), ctypes.c_int, ctypes.c_int],
            'posix_spawn_file_actions_addchdir_np': [pointer(_SpawnActions), ctypes.c_char_p],
            'posix_spawn_file_actions_addclosefrom_np': [pointer(_SpawnActions), ctypes.c_int],
        }
        try:
            for name, arguments in signatures.items():
                function = getattr(self.libc, name)
                function.argtypes = arguments
                function.restype = ctypes.c_int
        except AttributeError as error:
            raise RuntimeError('portal fixture requires Linux glibc pidfd_spawn and pidfd_getpid; '
                               'refusing non-atomic child creation') from error

    @staticmethod
    def _check(error):
        if error:
            raise OSError(error, os.strerror(error))

    def spawn(self, process, executable, env, cwd, log):
        argv = (ctypes.c_char_p * 2)(os.fsencode(executable), None)
        values = [os.fsencode(f'{key}={value}') for key, value in env.items()]
        if any(b'\0' in value for value in values):
            raise ValueError('embedded null byte in portal environment')
        envp = (ctypes.c_char_p * (len(values) + 1))(*values, None)
        attributes, actions, defaults = _SpawnAttributes(), _SpawnActions(), _Sigset()
        libc, check = self.libc, self._check
        check(libc.posix_spawnattr_init(ctypes.byref(attributes)))
        try:
            check(libc.posix_spawn_file_actions_init(ctypes.byref(actions)))
            try:
                # Match Popen's start_new_session and restore_signals behavior.
                check(libc.posix_spawnattr_setflags(ctypes.byref(attributes), 0x80 | 0x04))
                if libc.sigemptyset(ctypes.byref(defaults)):
                    check(ctypes.get_errno())
                for name in ('SIGPIPE', 'SIGXFZ', 'SIGXFSZ'):
                    if hasattr(signal, name) and libc.sigaddset(ctypes.byref(defaults), getattr(signal, name)):
                        check(ctypes.get_errno())
                check(libc.posix_spawnattr_setsigdefault(ctypes.byref(attributes), ctypes.byref(defaults)))
                for descriptor in (1, 2):
                    check(libc.posix_spawn_file_actions_adddup2(ctypes.byref(actions), log.fileno(), descriptor))
                # The newly opened log can itself be fd 0 when stdin was closed.
                check(libc.posix_spawn_file_actions_addopen(ctypes.byref(actions), 0, b'/dev/null', os.O_RDONLY, 0))
                check(libc.posix_spawn_file_actions_addchdir_np(ctypes.byref(actions), os.fsencode(cwd)))
                check(libc.posix_spawn_file_actions_addclosefrom_np(ctypes.byref(actions), 3))
                check(libc.pidfd_spawn(ctypes.byref(process.fd), os.fsencode(executable),
                                       ctypes.byref(actions), ctypes.byref(attributes), argv, envp))
            finally:
                libc.posix_spawn_file_actions_destroy(ctypes.byref(actions))
        finally:
            libc.posix_spawnattr_destroy(ctypes.byref(attributes))

    def pid(self, process):
        pid = self.libc.pidfd_getpid(process.fd.value)
        if pid < 0:
            self._check(ctypes.get_errno())
        return pid


@dataclass
class _Service:
    label: str
    name: str
    process: _PortalProcess
    starttime: str | None = None

    def identity(self):
        return {'pid': self.process.pid, 'starttime': self.starttime}


class PortalFixture:
    """Start two direct portal processes inside an existing owned display.

    root must be a new absolute directory with an existing parent and no symlink
    components. env supplies only PATH and LANG; display and bus handles always
    come from virtual_display.child_environment(). Linux glibc pidfd_spawn is
    required so the child has an exact custody handle from creation. Call close()
    in the driver's finally block even when start() raises. Failed startup also
    attempts its own bounded shutdown, retaining handles for incomplete cleanup.
    The caller keeps its native_interactions.Interrupts scope active through the
    final close(). The fixture preserves those handlers and their cancellation.
    """

    def __init__(self, root: Path, env: dict[str, str], interrupts: Interrupts):
        self.root = Path(root)
        self._source_env = {key: env[key] for key in ('PATH', 'LANG') if key in env}
        self.env = None
        self._services = []
        self._logs = []
        self._created = False
        self._attempted = False
        self._ready = False
        self._cleanup = None
        self._spawner = None
        self._interrupts = interrupts

    def _verify_bus(self):
        virtual_display.verify_owned_display()
        runtime = Path(os.environ['XDG_RUNTIME_DIR'])
        services = runtime / 'services'
        config = runtime / 'bus.conf'
        if (services.is_symlink() or not services.is_dir()
                or next(services.iterdir(), None) is not None):
            raise RuntimeError('portal fixture requires the wrapper\'s empty D-Bus service directory')
        expected = virtual_display.BUS_CONFIG.format(work=runtime, services=services)
        if (config.is_symlink() or config.stat().st_size > 8192
                or config.read_text() != expected):
            raise RuntimeError('private D-Bus configuration differs from virtual_display.py')
        address = os.environ['DBUS_SESSION_BUS_ADDRESS']
        if (runtime / 'bus').read_text() != address:
            raise RuntimeError('D-Bus address does not match the wrapper session record')
        if self.env is not None and any(self.env.get(key) != os.environ.get(key) for key in (
                'DISPLAY', 'XAUTHORITY', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS')):
            raise RuntimeError('portal display or bus handles changed after setup')

    def _prepare(self):
        self._verify_bus()
        _require_interrupts(self._interrupts)
        if (not hasattr(os, 'P_PIDFD') or not hasattr(os, 'waitid')
                or not hasattr(signal, 'pidfd_send_signal')):
            raise RuntimeError('portal fixture requires Linux pidfd waiting and pidfd_send_signal')
        self._spawner = _PidfdSpawner()
        for executable in (BACKEND, FRONTEND, BUSCTL):
            if not executable.is_file() or not os.access(executable, os.X_OK):
                raise RuntimeError(f'required installed executable is unavailable: {executable}')
        if not self.root.is_absolute() or self.root.resolve() != self.root:
            raise ValueError('portal root must be absolute and have no symlink components')
        self.root.mkdir(mode=0o700)
        self._created = True
        env = virtual_display.child_environment(self._source_env)
        for key, directory in (
                ('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'),
                ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state'), ('TMPDIR', 'tmp')):
            path = self.root / directory
            path.mkdir(mode=0o700)
            env[key] = str(path)
        config = self.root / 'config/xdg-desktop-portal'
        config.mkdir(mode=0o700)
        (config / 'portals.conf').write_text(PORTALS_CONFIG)
        env.update(GDK_BACKEND='x11', GTK_IM_MODULE='gtk-im-context-simple',
                   GSETTINGS_BACKEND='memory', GIO_USE_VFS='local',
                   GIO_USE_VOLUME_MONITOR='unix',
                   DBUS_SYSTEM_BUS_ADDRESS=f'unix:path={self.root}/no-system-bus')
        self.env = env

    def _call(self, method, argument, timeout=1.0):
        # Even the observation client must stay on the current owned display/bus.
        virtual_display.verify_owned_display()
        result = subprocess.run([
            str(BUSCTL), '--address=' + self.env['DBUS_SESSION_BUS_ADDRESS'],
            'call', 'org.freedesktop.DBus', '/org/freedesktop/DBus',
            'org.freedesktop.DBus', method, 's', argument,
        ], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True,
            timeout=timeout)
        if result.returncode:
            raise RuntimeError(f'private D-Bus observation failed: {method}, {argument}')
        return shlex.split(result.stdout)

    def _has_owner(self, name, timeout=1.0):
        value = self._call('NameHasOwner', name, timeout)
        if value not in (['b', 'true'], ['b', 'false']):
            raise RuntimeError(f'malformed private D-Bus ownership response: {name}')
        return value[1] == 'true'

    def _verify_owner(self, service, timeout=1.0):
        if service.process.poll() is not None:
            raise RuntimeError(f'{service.label} exited with {service.process.returncode}; see {service.label}.log')
        if process_identity(service.process.pid) != service.starttime:
            raise RuntimeError(f'{service.label} process identity changed')
        if not self._has_owner(service.name, timeout):
            return False
        owner = self._call('GetNameOwner', service.name, timeout)
        if len(owner) != 2 or owner[0] != 's' or not owner[1].startswith(':'):
            raise RuntimeError(f'malformed private D-Bus unique owner: {service.name}')
        pid = self._call('GetConnectionUnixProcessID', owner[1], timeout)
        if pid != ['u', str(service.process.pid)]:
            raise RuntimeError(f'{service.name} belongs to a process other than the started {service.label}')
        if (self._call('GetNameOwner', service.name, timeout) != owner
                or process_identity(service.process.pid) != service.starttime
                or service.process.poll() is not None):
            raise RuntimeError(f'{service.label} ownership changed during verification')
        return True

    def _reject_helpers(self):
        for name in UNAPPROVED_HELPERS:
            if self._has_owner(name):
                raise RuntimeError(f'unapproved helper owns {name} on the private bus')

    def _check_helper_errors(self):
        for log in self._logs:
            log.flush()
            with Path(log.name).open('rb') as source:
                source.seek(max(0, source.seek(0, os.SEEK_END) - 65536))
                lines = source.read(65536).decode('utf-8', 'replace').lower().splitlines()
            for line in lines:
                failed = any(word in line for word in ('failed', 'error', 'cannot', "couldn't", 'could not', 'not provided'))
                if not failed:
                    continue
                for name, terms in (
                        (UNAPPROVED_HELPERS[0], ('permissionstore', 'permission store')),
                        (UNAPPROVED_HELPERS[1], ('document portal', 'org.freedesktop.portal.documents'))):
                    if any(term in line for term in terms):
                        raise RuntimeError(f'portal startup requires unavailable {name}; '
                                           f'activation and extra helpers are disabled; see {Path(log.name).name}')

    def _launch(self, label, executable, name):
        self._verify_bus()
        log = (self.root / f'{label}.log').open('wb')
        self._logs.append(log)
        process = _PortalProcess(self._interrupts)
        service = _Service(label, name, process)
        self._services.append(service)
        self._spawner.spawn(process, executable, self.env, self.root, log)
        process.pid = self._spawner.pid(process)
        service.starttime = process_identity(process.pid)
        if service.starttime is None:
            raise RuntimeError(f'could not record {label} process starttime')
        deadline = time.monotonic() + READINESS_TIMEOUT
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f'{label} did not acquire {name} within {READINESS_TIMEOUT:g}s; '
                                   'PermissionStore/Documents activation is disabled; inspect the retained portal logs')
            self._check_helper_errors()
            # A frontend cannot qualify against a backend that died during startup.
            if all(self._verify_owner(item, min(1.0, remaining / 10)) for item in self._services):
                return
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))

    def start(self) -> dict:
        if self._attempted or self._cleanup is not None:
            raise RuntimeError('a PortalFixture instance can start only once')
        self._attempted = True
        try:
            self._prepare()
            for name in (BACKEND_NAME, FRONTEND_NAME, *UNAPPROVED_HELPERS):
                if self._has_owner(name):
                    raise RuntimeError(f'private portal bus name is already owned: {name}')
            self._launch('backend', BACKEND, BACKEND_NAME)
            self._launch('frontend', FRONTEND, FRONTEND_NAME)
            self._verify_bus()
            self._reject_helpers()
            self._check_helper_errors()
            if not all(self._verify_owner(service) for service in self._services):
                raise RuntimeError('portal lost its bus name before startup completed')
            receipt = {'status': 'ready', 'scope': 'service ownership only; picker workflow unqualified',
                       **{service.label: service.identity() for service in self._services}}
            (self.root / 'startup.json').write_text(json.dumps(receipt, indent=2) + '\n')
            self._ready = True
            return receipt
        except BaseException as error:
            failure = traceback.format_exc()
            try:
                cleanup = self.close()
                if cleanup['remainingPids'] or cleanup['errors']:
                    error.add_note('portal cleanup is incomplete; inspect cleanup.json and retain the fixture')
            except BaseException as cleanup_error:
                error.add_note(f'portal cleanup also failed: {cleanup_error}')
            if self._created:
                try:
                    (self.root / 'startup-failed.txt').write_text(failure)
                except OSError as receipt_error:
                    error.add_note(f'could not retain portal startup failure: {receipt_error}')
            raise

    def picker_application(self):
        """Return the owned backend's AT-SPI application, or None before it appears."""
        if not self._ready or self._cleanup is not None:
            raise RuntimeError('portal fixture is not running')
        if not all(self._verify_owner(service) for service in self._services):
            raise RuntimeError('portal service no longer owns its bus name')
        self._check_helper_errors()
        from native_adapter import _atspi

        desktop = _atspi().get_desktop(0)
        pid = self._services[0].process.pid
        matches = [node for index in range(desktop.get_child_count())
                   if (node := desktop.get_child_at_index(index)) is not None
                   and node.get_process_id() == pid]
        if len(matches) > 1:
            raise RuntimeError('multiple AT-SPI applications match the owned GTK backend')
        return matches[0] if matches else None

    def close(self) -> dict:
        if self._cleanup is not None and all(not service.process.started or service.process.closed
                                             for service in self._services):
            return self._cleanup
        errors = []
        for service in reversed(self._services):
            process = service.process
            if not process.started:
                continue
            try:
                if process.poll() is None:
                    # Atomic creation already proves custody if PID/starttime
                    # recording itself failed before readiness.
                    if service.starttime is not None and process_identity(process.pid) != service.starttime:
                        raise RuntimeError(f'{service.label} identity changed; refusing to signal it')
                    signal.pidfd_send_signal(process.fd.value, signal.SIGTERM)
                    try:
                        process.wait(timeout=STOP_TIMEOUT)
                    except subprocess.TimeoutExpired:
                        if service.starttime is not None and process_identity(process.pid) != service.starttime:
                            raise RuntimeError(f'{service.label} identity changed before forced shutdown')
                        signal.pidfd_send_signal(process.fd.value, signal.SIGKILL)
                        process.wait(timeout=STOP_TIMEOUT)
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                errors.append(str(error))
            finally:
                if process.returncode is not None and not process.closed:
                    try:
                        _require_interrupts(self._interrupts)
                        with self._interrupts.defer():
                            try:
                                os.close(process.fd.value)
                            except OSError as error:
                                errors.append(str(error))
                            finally:
                                # Linux relinquishes the descriptor even when
                                # close reports an error. Never retry its number.
                                process.closed = True
                    except RuntimeError as error:
                        errors.append(str(error))
        for log in self._logs:
            try:
                log.close()
            except OSError as error:
                errors.append(str(error))
        self._ready = False
        services = [service for service in self._services if service.process.started]
        receipt = {'remainingPids': [service.process.pid for service in self._services
                                     if service.process.started and service.process.returncode is None],
                   'errors': errors,
                   'processes': [{**service.identity(), 'exitCode': service.process.returncode}
                                 for service in services]}
        self._cleanup = receipt
        if self._created:
            (self.root / 'cleanup.json').write_text(json.dumps(receipt, indent=2) + '\n')
        return receipt
