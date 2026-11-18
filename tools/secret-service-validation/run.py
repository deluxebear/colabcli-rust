#!/usr/bin/env python3
"""Private D-Bus/Secret Service validation with synthetic data only."""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import keyring
from keyring.backends.SecretService import Keyring
import secretstorage
from client import guard

ROOT = Path('/workspace')
OUT = Path('/validation/reports')
CONTROL = Path('/validation/home/runtime/keyring')
PASSWORD = b'synthetic-disposable-keyring-password'
TESTS = {
    'healthy': 'native_secret_service_interoperates_with_python_and_legacy_slots',
    'locked': 'native_secret_service_locked_collection_is_a_bounded_error',
    'timeout': 'native_secret_service_unresponsive_bus_has_a_bounded_caller_wait',
    'outage': 'native_secret_service_outage_preserves_strict_and_auto_storage_contracts',
    'recovered': 'native_secret_service_recovery_keeps_file_authoritative_until_explicit_keyring_write',
}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inputs():
    paths = [ROOT / name for name in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'README.md')]
    for directory in ('src', 'tests', 'resources'):
        paths.extend(p for p in (ROOT / directory).rglob('*') if p.is_file())
    paths.extend((ROOT / 'tools').rglob('*.py'))
    paths.extend((ROOT / 'tools/secret-service-validation').glob('*'))
    return {str(p.relative_to(ROOT)): sha(p) for p in sorted(set(paths)) if p.is_file()}


def bus_call(method, value):
    result = subprocess.run(['dbus-send', '--session', '--print-reply', '--dest=org.freedesktop.DBus',
                             '/org/freedesktop/DBus', 'org.freedesktop.DBus.' + method, 'string:' + value],
                            capture_output=True, text=True, timeout=3, check=True)
    return result.stdout


def main():
    guard()
    if os.environ.get('CARGO_NET_OFFLINE') != 'true':
        raise RuntimeError('offline dependency cache required')
    OUT.mkdir(parents=True, exist_ok=True)
    CONTROL.mkdir(mode=0o700, parents=True, exist_ok=True)
    before = inputs()
    result = {'schema_version': 1, 'passed': False, 'source_hashes': before, 'stages': [],
              'backend': 'GNOME Keyring Secret Service on a private Unix D-Bus',
              'synthetic_data_only': True, 'limitations': ['single Linux ARM64 container only',
                  'no host desktop keyring, macOS or Windows interoperability',
                  'no Google authorization or OAuth token acceptance',
                  'no concurrent Python/Rust configuration-file writers']}
    daemon = None
    bus_pid = int(re.search(r'uint32\s+(\d+)', bus_call('GetConnectionUnixProcessID', 'org.freedesktop.DBus')).group(1))
    assert b'dbus-daemon' in Path(f'/proc/{bus_pid}/cmdline').read_bytes()
    assert Path(f'/proc/{bus_pid}').stat().st_uid == os.geteuid()
    assert bus_pid != os.getpid()
    paused = False
    keyring.set_keyring(Keyring())

    def stage(name, command, phase=None):
        print('starting ' + name, flush=True)
        path = OUT / (name + '.log')
        started = time.monotonic()
        with path.open('wb') as log:
            env = dict(os.environ)
            if phase:
                env['COLAB_NATIVE_STORE_TEST_PHASE'] = phase
            try:
                code = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, env=env, timeout=600).returncode
            except subprocess.TimeoutExpired:
                code = 124
        row = {'name': name, 'command': command, 'exit_code': code,
               'duration_seconds': round(time.monotonic() - started, 3), 'log_sha256': sha(path)}
        text = path.read_text(errors='replace')
        counts = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', text)
        if counts:
            row.update(tests_passed=sum(int(p) for p, _, _ in counts), tests_ignored=sum(int(i) for _, _, i in counts))
        if name.endswith('version'):
            row['version'] = text.strip()
        result['stages'].append(row)
        (OUT / 'secret-service-validation.json').write_text(json.dumps(result, indent=2) + '\n')
        print('finished ' + name + ': ' + str(code), flush=True)
        if phase:
            assert counts and row['tests_passed'] == 1 and row['tests_ignored'] == 0 and code == 0, 'native Secret Service stage failed'
        if code:
            raise RuntimeError('validation command failed')

    def start():
        nonlocal daemon
        log = (OUT / 'daemon.log').open('ab')
        daemon = subprocess.Popen(['gnome-keyring-daemon', '--foreground', '--unlock', '--components=secrets',
                                    '--control-directory=' + str(CONTROL)], stdout=log, stderr=log, stdin=subprocess.PIPE)
        daemon.stdin.write(PASSWORD)
        daemon.stdin.close()
        log.close()
        deadline = time.monotonic() + 8
        while 'boolean true' not in bus_call('NameHasOwner', 'org.freedesktop.secrets'):
            if daemon.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError('private Secret Service failed to start')
            time.sleep(0.05)
        deadline = time.monotonic() + 8
        while True:
            collections = list(secretstorage.get_all_collections(secretstorage.dbus_init()))
            if any(collection.collection_path.endswith('/login') and not collection.is_locked() for collection in collections):
                break
            if daemon.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError('synthetic login collection did not unlock')
            time.sleep(0.05)
        assert not secretstorage.get_default_collection(secretstorage.dbus_init()).is_locked()

    def stop():
        nonlocal daemon
        if daemon is not None:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=5)
            daemon = None
        deadline = time.monotonic() + 5
        while 'boolean true' in bus_call('NameHasOwner', 'org.freedesktop.secrets'):
            if time.monotonic() >= deadline:
                raise RuntimeError('private service still owns the bus name')
            time.sleep(0.05)

    def test(phase):
        stage(phase, ['cargo', 'test', '--locked', '--test', 'secret_service', TESTS[phase], '--', '--exact', '--ignored'], phase)

    try:
        stage('rust-version', ['rustc', '--version'])
        stage('python-keyring-version', ['python3', '-c', 'import importlib.metadata as m; print(m.version("keyring"))'])
        stage('gnome-keyring-version', ['gnome-keyring-daemon', '--version'])
        stage('fmt', ['cargo', 'fmt', '--check'])
        stage('msrv', ['cargo', '+1.85.1', 'check', '--locked', '--test', 'secret_service'])
        stage('clippy', ['cargo', 'clippy', '--locked', '--test', 'secret_service', '--', '-D', 'warnings'])
        stage('disabled-by-default', ['cargo', 'test', '--locked', '--test', 'secret_service'])
        assert result['stages'][-1]['tests_ignored'] == 5
        start()
        test('healthy')
        keyring.set_password('google-colab-cli', 'oauth_token:locked-probe', 'synthetic-locked-sentinel')
        secretstorage.get_default_collection(secretstorage.dbus_init()).lock()
        assert secretstorage.get_default_collection(secretstorage.dbus_init()).is_locked()
        test('locked')
        os.kill(bus_pid, signal.SIGSTOP)
        paused = True
        try:
            test('timeout')
        finally:
            os.kill(bus_pid, signal.SIGCONT)
            paused = False
        stop()
        test('outage')
        start()
        test('recovered')
        keyring.delete_password('google-colab-cli', 'oauth_token:locked-probe')
        result['source_unchanged'] = before == inputs()
        result['passed'] = result['source_unchanged']
        return 0 if result['passed'] else 1
    finally:
        if paused:
            os.kill(bus_pid, signal.SIGCONT)
        stop()
        result['owned_daemon_stopped'] = daemon is None
        (OUT / 'secret-service-validation.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    raise SystemExit(main())
