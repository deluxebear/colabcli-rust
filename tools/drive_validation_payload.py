"""Standalone stdlib-only payload embedded by prepare_live_drive_validation.py."""
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time


class ProbeError(RuntimeError):
    pass


OWNER = '.colabcli-owner.json'
DATA = 'probe-雪.bin'
PAYLOAD = bytes(range(256)) * 4 + 'Colab Drive 验收\n'.encode()


def verify_mount(path, descriptor):
    """Check the opened directory against Linux's actual FUSE mount table."""
    device = os.fstat(descriptor).st_dev
    identity = f'{os.major(device)}:{os.minor(device)}'
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        fields = line.split()
        if len(fields) > 6 and fields[2] == identity and fields[4] == path:
            separator = fields.index('-')
            if fields[separator + 1].startswith('fuse'):
                return
    raise ProbeError('expected_fuse_mount_not_present')


def open_directory(name, parent=None):
    return os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)


def read_file(directory, name, limit):
    descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
    with os.fdopen(descriptor, 'rb') as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ProbeError('unexpected_file_type')
        value = stream.read(limit + 1)
        if len(value) > limit:
            raise ProbeError('file_exceeds_probe_limit')
        return value


def create_file(directory, name, value):
    descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=directory)
    with os.fdopen(descriptor, 'wb') as stream:
        stream.write(value)
        stream.flush()
        os.fsync(stream.fileno())


def owner_value(nonce, folder):
    return {'version': 1, 'nonce': nonce, 'folder': folder,
            'payload_sha256': hashlib.sha256(PAYLOAD).hexdigest()}


def check_files(directory, expected, allow_partial_cleanup=False, wait_for_created=False):
    # A retained directory descriptor can carry an old enumeration position.
    # Rewind the same verified descriptor; never reopen through a mutable path.
    deadline = time.monotonic() + 5
    while True:
        os.lseek(directory, 0, os.SEEK_SET)
        entries = set(os.listdir(directory))
        if (not wait_for_created or entries == {OWNER, DATA}
                or not entries.issubset({OWNER, DATA}) or time.monotonic() >= deadline):
            break
        # DriveFS can expose a newly created file before its directory listing
        # catches up. Only observe again; never repeat creation or ignore extras.
        time.sleep(.1)
    accepted = ({OWNER, DATA}, {OWNER}) if allow_partial_cleanup else ({OWNER, DATA},)
    if entries not in accepted:
        raise ProbeError('unexpected_directory_entries')
    # The probe wrote this exact serialization. Byte comparison also rejects
    # duplicate JSON keys and Python's True == 1 equality ambiguity.
    if read_file(directory, OWNER, 4096) != json.dumps(expected, sort_keys=True).encode():
        raise ProbeError('owner_marker_mismatch')
    if DATA in entries and read_file(directory, DATA, len(PAYLOAD)) != PAYLOAD:
        raise ProbeError('payload_changed')
    return entries


def perform(plan, phase):
    nonce, path, folder = plan['nonce'], plan['mountpoint'], plan['folder']
    if (not isinstance(nonce, str) or not re.fullmatch('[a-f0-9]{32}', nonce)
            or not isinstance(path, str) or not os.path.isabs(path)
            or os.path.normpath(path) != path
            or os.path.basename(path) != 'colabcli-drive-' + nonce
            or folder != 'colabcli-validation-' + nonce
            or phase not in ('create', 'readonly', 'cleanup')):
        raise ProbeError('probe_plan_invalid')
    expected = owner_value(nonce, folder)
    mount = open_directory(path)
    root = directory = None
    try:
        verify_mount(path, mount)
        root = open_directory('MyDrive', mount)
        if phase == 'create':
            # Existing paths, including this probe's previous unknown result,
            # must be inspected by the caller instead of blindly replayed.
            os.mkdir(folder, mode=0o700, dir_fd=root)
        try:
            directory = open_directory(folder, root)
        except FileNotFoundError:
            if phase == 'cleanup':
                return {'phase': phase, 'status': 'already_absent', 'live_identity_verified': False}
            raise
        if phase == 'create':
            create_file(directory, OWNER, json.dumps(expected, sort_keys=True).encode())
            create_file(directory, DATA, PAYLOAD)
        entries = check_files(directory, expected, allow_partial_cleanup=phase == 'cleanup', wait_for_created=phase == 'create')
        if phase == 'readonly':
            denied = False
            descriptor = None
            try:
                descriptor = os.open(DATA, os.O_WRONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
                if not stat.S_ISREG(os.fstat(descriptor).st_mode):
                    raise ProbeError('unexpected_file_type')
                # Same byte as the verified sample: even an unexpectedly
                # writable mount does not change the sample's content.
                if os.pwrite(descriptor, PAYLOAD[:1], 0) != 1:
                    raise ProbeError('unexpected_write_result')
            except OSError as error:
                if error.errno not in (errno.EROFS, errno.EACCES, errno.EPERM):
                    raise
                denied = True
            finally:
                if descriptor is not None:
                    os.close(descriptor)
            check_files(directory, expected)
            if not denied:
                raise ProbeError('readonly_write_unexpectedly_allowed')
            return {'phase': phase, 'status': 'write_denied', 'original_bytes_unchanged': True,
                    'live_identity_verified': False}
        if phase == 'cleanup':
            if DATA in entries:
                os.unlink(DATA, dir_fd=directory)
            os.unlink(OWNER, dir_fd=directory)
            os.rmdir(folder, dir_fd=root)  # Refuses newly added entries; no recursion.
            return {'phase': phase, 'status': 'removed', 'live_identity_verified': False}
        return {'phase': phase, 'status': 'created_and_read_back', 'payload_bytes': len(PAYLOAD),
                'live_identity_verified': False}
    finally:
        for descriptor in (directory, root, mount):
            if descriptor is not None:
                os.close(descriptor)


if __name__ == '__main__':
    phase = globals().get('PHASE', 'unprepared')
    try:
        result = perform(PLAN, phase)
    except Exception as error:
        print(json.dumps({'phase': phase, 'status': 'failed',
                          'error': str(error) if isinstance(error, ProbeError) else type(error).__name__}))
        raise SystemExit(1)
    print(json.dumps(result))
