"""Read current-user /tmp quota on this Linux host, without changing quota state."""

import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import time

HEADERS = (Path('/usr/include/asm/unistd_64.h'), Path('/usr/include/linux/quota.h'))
FIELDS = ('dqb_bhardlimit', 'dqb_bsoftlimit', 'dqb_curspace', 'dqb_ihardlimit',
          'dqb_isoftlimit', 'dqb_curinodes', 'dqb_btime', 'dqb_itime', 'dqb_valid')
Q_GETQUOTA = 0x800007
SYS_QUOTACTL_FD = 443


class _Quota(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint64) for name in FIELDS[:-1]] + [(FIELDS[-1], ctypes.c_uint32)]


def headroom_bytes(hard_kib, soft_kib, usage_bytes, valid):
    """Require valid finite block limits/usage and use the smaller nonzero limit."""
    if valid & 3 != 3 or min(hard_kib, soft_kib, usage_bytes) < 0:
        raise RuntimeError('Quota block limits or byte usage are unknown')
    limits = [value * 1024 for value in (hard_kib, soft_kib) if value]
    if not limits:
        raise RuntimeError('No finite current-user quota limit is available')
    return max(0, min(limits) - usage_bytes)


def _header_evidence():
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        raise RuntimeError('This read-only quota implementation requires Linux x86_64')
    raw = {path: path.read_bytes() for path in HEADERS}
    syscall_header, quota_header = (raw[path].decode() for path in HEADERS)
    syscall = re.search(r'^#define __NR_quotactl_fd\s+(\d+)\s*$', syscall_header, re.M)
    command = re.search(r'^#define Q_GETQUOTA\s+(0x[0-9a-fA-F]+)\b', quota_header, re.M)
    shift = re.search(r'^#define SUBCMDSHIFT\s+(\d+)\b', quota_header, re.M)
    user = re.search(r'^#define USRQUOTA\s+(\d+)\b', quota_header, re.M)
    if (not syscall or int(syscall[1]) != SYS_QUOTACTL_FD
            or not command or int(command[1], 16) != Q_GETQUOTA
            or not shift or int(shift[1]) != 8 or not user or int(user[1]) != 0):
        raise RuntimeError('Installed headers do not match the verified read-only quota ABI')
    structure = re.search(r'struct if_dqblk\s*\{([^}]+)\}', quota_header, re.S)
    observed = re.findall(r'__u(64|32)\s+(dqb_\w+)\s*;', structure[1] if structure else '')
    if observed != [('64', name) for name in FIELDS[:-1]] + [('32', FIELDS[-1])]:
        raise RuntimeError('Installed quota structure differs from the verified ABI')
    if ctypes.sizeof(_Quota) != 72:
        raise RuntimeError('Unexpected quota structure size')
    return {str(path): hashlib.sha256(value).hexdigest() for path, value in raw.items()}


def _tmp_mount():
    matches = []
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        before, after = line.split(' - ', 1)
        fields, filesystem = before.split(), after.split()
        if fields[4] == '/tmp':
            matches.append((line, fields, filesystem))
    if len(matches) != 1:
        raise RuntimeError('Expected one unambiguous /tmp mount')
    line, fields, filesystem = matches[0]
    options = set(fields[5].split(',')) | set(filesystem[2].split(','))
    if (filesystem[0] != 'tmpfs' or 'usrquota' not in options
            or 'rw' not in fields[5].split(',') or 'ro' in options):
        raise RuntimeError('/tmp is not the expected writable user-quota tmpfs')
    return {'record': line, 'id': fields[0], 'device': fields[2],
            'fstype': filesystem[0], 'mountpoint': fields[4]}


def sample_tmp_quota():
    """Sample only Q_GETQUOTA for this UID and filesystem availability on /tmp."""
    if os.getuid() != os.geteuid():
        raise RuntimeError('Quota reads must use the real current-user identity')
    headers = _header_evidence()
    mount = _tmp_mount()
    quota = _Quota()
    library = ctypes.CDLL(None, use_errno=True)
    library.syscall.restype = ctypes.c_long
    descriptor = os.open('/tmp', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        device = os.fstat(descriptor).st_dev
        if f'{os.major(device)}:{os.minor(device)}' != mount['device']:
            raise RuntimeError('The opened directory is not the verified /tmp mount')
        result = library.syscall(ctypes.c_long(SYS_QUOTACTL_FD), ctypes.c_int(descriptor),
                                 ctypes.c_uint(Q_GETQUOTA << 8), ctypes.c_uint(os.getuid()),
                                 ctypes.byref(quota))
        if result != 0:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error))
        filesystem = os.fstatvfs(descriptor)
        if _tmp_mount() != mount:
            raise RuntimeError('/tmp mount changed during the quota sample')
    finally:
        os.close(descriptor)
    return {'uid': os.getuid(), 'method': 'quotactl_fd:Q_GETQUOTA:USRQUOTA',
            'read_only': True, 'headers': headers, 'mount': mount,
            'sampled_monotonic_ns': time.monotonic_ns(), 'valid': quota.dqb_valid,
            'hard_kib': quota.dqb_bhardlimit, 'soft_kib': quota.dqb_bsoftlimit,
            'usage_bytes': quota.dqb_curspace,
            'headroom_bytes': headroom_bytes(quota.dqb_bhardlimit, quota.dqb_bsoftlimit,
                                             quota.dqb_curspace, quota.dqb_valid),
            'filesystem_available_bytes': filesystem.f_bavail * filesystem.f_frsize}


if __name__ == '__main__':
    try:
        print(json.dumps({'ok': True, **sample_tmp_quota()}, indent=2))
    except (OSError, RuntimeError) as error:
        print(json.dumps({'ok': False, 'error': f'{type(error).__name__}: {error}'}))
        raise SystemExit(2)
