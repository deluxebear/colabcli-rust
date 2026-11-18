#!/usr/bin/env python3
"""Run locked offline Linux validation inside the dedicated non-root container."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import time

ROOT = Path('/workspace')
REPORTS = Path('/validation/reports')
TARGET = Path('/validation/target')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_hashes():
    paths = [ROOT / name for name in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'README.md', 'THIRD_PARTY_NOTICES.md')]
    for directory in ('src', 'tests', 'resources', 'licenses'):
        paths.extend(p for p in (ROOT / directory).rglob('*') if p.is_file())
    paths.extend((ROOT / 'tools').rglob('*.py'))
    paths.extend((ROOT / '.github/workflows').glob('*.yml'))
    paths.append(ROOT / 'tools/linux-validation/Dockerfile')
    return {str(p.relative_to(ROOT)): digest(p) for p in sorted(paths)}


def execution_architecture(version, machine, expected=None):
    hosts = re.findall(r'^host: (.+)$', version, re.MULTILINE)
    supported = {'x86_64': 'x86_64-unknown-linux-gnu', 'aarch64': 'aarch64-unknown-linux-gnu'}
    if machine not in supported or hosts != [supported[machine]] or (expected and machine != expected):
        raise ValueError('runtime/toolchain/expected Linux architecture mismatch')
    return hosts[0]


def elf_machine(path):
    with path.open('rb') as stream:
        header = stream.read(20)
    if len(header) != 20 or header[:6] != b'\x7fELF\x02\x01':
        raise ValueError('expected 64-bit little-endian ELF executable')
    value = int.from_bytes(header[18:20], 'little')
    if value not in (62, 183):
        raise ValueError('unexpected ELF machine')
    return {62: 'x86_64', 183: 'aarch64'}[value]


def rust_test_totals(log):
    counts = re.findall(r'test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored', log)
    return {'tests_passed': sum(int(p) for p, _, _ in counts),
            'tests_failed': sum(int(f) for _, f, _ in counts),
            'tests_ignored': sum(int(i) for _, _, i in counts),
            'test_summaries': len(counts)}


def main():
    if sys.platform != 'linux' or os.geteuid() == 0:
        raise SystemExit('Linux validation must run as a non-root container user')
    if os.environ.get('CARGO_NET_OFFLINE') != 'true' or Path(os.environ.get('HOME', '')) != Path('/validation/home'):
        raise SystemExit('isolated home and offline Cargo are required')
    if any(key.startswith(('GOOGLE_', 'AWS_', 'COLAB_')) for key in os.environ):
        raise SystemExit('authentication or application environment must not be inherited')
    REPORTS.mkdir(parents=True, exist_ok=True)
    version = subprocess.run(['rustc', '-vV'], check=True, capture_output=True, text=True).stdout
    machine = platform.machine()
    rust_host = execution_architecture(version, machine, os.environ.get('VALIDATION_EXPECTED_MACHINE'))
    before = source_hashes()
    result = {
        'schema_version': 1,
        'platform': {'system': platform.system(), 'machine': machine, 'uid': os.geteuid(), 'rust_host': rust_host},
        'source_hashes': before,
        'isolation': {'home': '/validation/home', 'cargo_offline': True,
                      'required_network_mode': 'none', 'host_credentials_mounted': False,
                      'network_and_mounts_must_be_verified_by_launcher': True},
        'stages': [], 'passed': False,
        'limitations': ['container validation, not a desktop Linux session',
                        'no real Secret Service or Python keyring interoperability',
                        'no Google OAuth, ADC, Drive or Colab requests',
                        'no remote GitHub download or publication',
                        'single Linux architecture and Debian image only'],
    }
    commands = [
        ('rust-version', ['rustc', '--version']),
        ('msrv-version', ['rustc', '+1.85.1', '--version']),
        ('cargo-version', ['cargo', '--version']),
        ('openssh-version', ['ssh', '-V']),
        ('msrv-default', ['cargo', '+1.85.1', 'check', '--locked', '--all-targets']),
        ('msrv-no-default', ['cargo', '+1.85.1', 'check', '--locked', '--no-default-features', '--all-targets']),
        ('fmt', ['cargo', 'fmt', '--check']),
        ('clippy-default', ['cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings']),
        ('test-default', ['cargo', 'test', '--locked']),
        ('test-no-default', ['cargo', 'test', '--locked', '--no-default-features']),
        ('clippy-no-default', ['cargo', 'clippy', '--locked', '--no-default-features', '--all-targets', '--', '-D', 'warnings']),
        ('python-tools', ['python3', '-B', '-m', 'unittest', 'discover', '-s', 'tools/tests']),
        ('restore-default', ['cargo', 'test', '--locked', '--no-run']),
        ('build-release', ['cargo', 'build', '--release', '--locked']),
        ('native-release', ['python3', '-B', 'tools/validate_native_release.py', '--binary', str(TARGET / 'release/colab'),
                            '--debug-binary', str(TARGET / 'debug/colab'), '--output', str(REPORTS / 'native-release-linux.json')]),
    ]
    try:
        for name, command in commands:
            print('starting ' + name, flush=True)
            path = REPORTS / (name + '.log')
            started = time.monotonic()
            with path.open('wb') as output:
                code = subprocess.run(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=False).returncode
            row = {'name': name, 'command': command, 'exit_code': code,
                   'duration_seconds': round(time.monotonic() - started, 3), 'log_sha256': digest(path)}
            log = path.read_text(errors='replace')
            if name in ('test-default', 'test-no-default'):
                row.update(rust_test_totals(log))
                if code == 0 and (not row['test_summaries'] or row['tests_failed']):
                    code = 1
                    row['exit_code'] = code
            if name == 'python-tools':
                count = re.search(r'Ran (\d+) tests? in ', log)
                skips = re.search(r'OK \(skipped=(\d+)\)', log)
                row.update(tests_run=int(count[1]) if count else 0,
                           tests_skipped=int(skips[1]) if skips else 0)
                if code == 0 and (not count or 'OK' not in log):
                    code = 1
                    row['exit_code'] = code
            if name.endswith('-version'):
                row['version'] = log.strip()
            result['stages'].append(row)
            (REPORTS / 'linux-validation.json').write_text(json.dumps(result, indent=2) + '\n')
            print('finished ' + name + ': ' + str(code), flush=True)
            if code:
                return code
        result['source_unchanged'] = source_hashes() == before
        result['passed'] = False
        for name in ('debug', 'release'):
            binary = TARGET / name / 'colab'
            binary_machine = elf_machine(binary)
            if binary_machine != machine:
                raise ValueError('executed toolchain and output ELF architecture mismatch')
            result[name + '_binary'] = {'size': binary.stat().st_size, 'sha256': digest(binary),
                                       'elf_machine': binary_machine}
        result['passed'] = result['source_unchanged']
        return 0 if result['passed'] else 1
    finally:
        (REPORTS / 'linux-validation.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    sys.exit(main())
