#!/usr/bin/env python3
"""Build/test x86_64 macOS through Rosetta; never claim native Intel validation."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time
from urllib.parse import parse_qs, urlsplit
from package_release import package
from validate_live_ssh import terminate_group

ROOT = Path(__file__).resolve().parents[1]
TARGET = 'x86_64-apple-darwin'


def checksum(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_hashes():
    paths = [ROOT / name for name in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'README.md')]
    for directory in ('src', 'tests', 'resources'):
        paths.extend(path for path in (ROOT / directory).rglob('*') if path.is_file())
    paths += [Path(__file__).resolve(), ROOT / 'tools/package_release.py', ROOT / 'tools/validate_live_ssh.py']
    return {str(path.relative_to(ROOT)): checksum(path) for path in sorted(paths)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--toolchain', default='stable')
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'target/rosetta-validation')
    args = parser.parse_args()
    if platform.system() != 'Darwin' or platform.machine() not in ('arm64', 'aarch64'):
        parser.error('requires an Apple Silicon macOS host with Rosetta already installed')
    translated = subprocess.check_output(['/usr/bin/arch', '-x86_64', '/usr/sbin/sysctl', '-in', 'sysctl.proc_translated'], text=True).strip()
    if translated != '1':
        parser.error('x86_64 process was not confirmed translated')
    tools = {name: Path(subprocess.check_output(['rustup', 'which', '--toolchain', args.toolchain, name], text=True).strip())
             for name in ('cargo', 'rustc', 'rustdoc')}
    output = args.output_dir.resolve()
    if not output.is_relative_to(ROOT / 'target'):
        parser.error('output must be a new directory under the ignored project target directory')
    output.mkdir(parents=True, mode=0o700)
    before = source_hashes()
    result = {'schema_version': 1, 'target': TARGET, 'host_machine': platform.machine(),
              'execution_mode': 'rosetta', 'translated_process_confirmed': True,
              'rust_version': subprocess.check_output([str(tools['rustc']), '--version'], text=True).strip(),
              'cargo_version': subprocess.check_output([str(tools['cargo']), '--version'], text=True).strip(),
              'source_hashes': before, 'stages': [], 'passed': False, 'published': False,
              'limitations': ['translation on Apple Silicon; not native Intel hardware',
                              'no real keychain, Google, ADC or Drive authentication',
                              'partial single-target package; no remote release download',
                              'no signing/notarization or minimum macOS acceptance test']}
    scratch = Path(tempfile.mkdtemp(prefix='rosetta-work-', dir=ROOT / 'target'))
    try:
        original_home = Path.home()
        home = scratch / 'home'
        home.mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('COLAB_', 'GOOGLE_', 'AWS_')) and key not in
               ('CLOUDSDK_CONFIG', 'SSH_AUTH_SOCK', 'SSH_ASKPASS', 'DISPLAY', 'RUSTFLAGS',
                'CARGO_ENCODED_RUSTFLAGS', 'MACOSX_DEPLOYMENT_TARGET')}
        env.update(HOME=str(home), CARGO_HOME=str(original_home / '.cargo'),
                   RUSTUP_HOME=str(original_home / '.rustup'), RUSTUP_TOOLCHAIN=args.toolchain,
                   RUSTC=str(tools['rustc']), RUSTDOC=str(tools['rustdoc']),
                   PATH=str(tools['cargo'].parent) + os.pathsep + os.environ.get('PATH', os.defpath),
                   CARGO_TARGET_DIR=str(scratch / 'build'), CARGO_BUILD_JOBS='2',
                   RUST_TEST_THREADS='4', CARGO_NET_OFFLINE='true', CARGO_TERM_COLOR='never',
                   CARGO_ENCODED_RUSTFLAGS='', PYTHONDONTWRITEBYTECODE='1',
                   CARGO_TARGET_X86_64_APPLE_DARWIN_RUNNER='/usr/bin/arch -x86_64')
        cargo = str(tools['cargo'])
        commands = [
            ('clippy-default', [cargo, 'clippy', '--locked', '--all-targets', '--target', TARGET, '--', '-D', 'warnings']),
            ('test-default', [cargo, 'test', '--locked', '--target', TARGET]),
            ('clippy-no-default', [cargo, 'clippy', '--locked', '--no-default-features', '--all-targets', '--target', TARGET, '--', '-D', 'warnings']),
            ('test-no-default', [cargo, 'test', '--locked', '--no-default-features', '--target', TARGET]),
            ('restore-default', [cargo, 'test', '--locked', '--no-run', '--target', TARGET]),
            ('build-release', [cargo, 'build', '--release', '--locked', '--target', TARGET]),
        ]
        for name, command in commands:
            print('starting ' + name, flush=True)
            log = output / (name + '.log')
            started = time.monotonic()
            with log.open('wb') as stream:
                process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=stream,
                                           stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    code = process.wait(timeout=1200)
                except BaseException:
                    terminate_group(process)
                    raise
            result['stages'].append({'name': name, 'command': command, 'exit_code': code,
                                     'seconds': round(time.monotonic() - started, 3), 'log_sha256': checksum(log)})
            (output / 'validation.json').write_text(json.dumps(result, indent=2) + '\n')
            print('finished ' + name + ': ' + str(code), flush=True)
            if code:
                return code
        binary = scratch / 'build' / TARGET / 'release/colab'
        manifest = package(ROOT / 'Cargo.toml', [TARGET + '=' + str(binary)],
                           'https://github.com/deluxebear/colabcli-rust/releases/download/v0.1.0',
                           output / 'package', allow_partial=True)
        artifact = output / 'package' / Path(urlsplit(manifest['artifacts'][TARGET]['url']).path).name
        assert checksum(artifact) == checksum(binary) == manifest['artifacts'][TARGET]['sha256']
        assert artifact.stat().st_size == manifest['artifacts'][TARGET]['size']
        runtime_env = {'PATH': os.defpath, 'HOME': str(home)}
        def run(arguments):
            command = ['/usr/bin/arch', '-x86_64', str(artifact), *arguments]
            return subprocess.check_output(command, env=runtime_env, text=True, timeout=15)
        assert run(['version']).strip() == 'Version: ' + manifest['version']
        for shell in ('bash', 'zsh', 'fish', 'powershell', 'pwsh'):
            assert 'colab' in run(['--show-completion', shell])
        prefix = ['--config-dir', str(home / 'config'), '--token-storage', 'file']
        inventory = json.loads(run([*prefix, 'account', 'list', '--json']))
        assert inventory['active'] == 'default' and inventory['accounts'][0]['auth']['ready'] is False
        url = run([*prefix, '-a', 'translated-drive', 'drive', 'consent', '--url-only', '--no-browser',
                   '--flavor', 'custom', '--client-id', 'synthetic-rosetta-client',
                   '--auth-uri', 'http://127.0.0.1:9/auth', '--token-uri', 'http://127.0.0.1:9/token'])
        query = parse_qs(urlsplit(url.strip()).query)
        pending = json.loads((home / 'config/accounts/translated-drive/dfs_pending.json').read_text())
        challenge = base64.urlsafe_b64encode(hashlib.sha256(pending['code_verifier'].encode()).digest()).rstrip(b'=').decode()
        assert pending['account'] == 'translated-drive' and pending['state'] == query['state'][0]
        assert query['code_challenge_method'] == ['S256'] and query['code_challenge'] == [challenge]
        assert json.loads((home / 'config/accounts.json').read_text())['active'] == 'default'
        result['artifact'] = {'path': str(artifact.relative_to(ROOT)), 'size': artifact.stat().st_size, 'sha256': checksum(artifact)}
        result['packaged_checks'] = ['x86_64 Mach-O architecture', 'manifest size/SHA-256 and identical package bytes',
                                     'version through Rosetta', 'five completion scripts', 'offline file account readiness',
                                     'new Drive profile, unchanged active and matching state/S256']
        result['source_unchanged'] = source_hashes() == before
        result['passed'] = result['source_unchanged']
        return 0 if result['passed'] else 1
    except Exception as error:
        result['error_class'] = type(error).__name__
        raise
    finally:
        shutil.rmtree(scratch)
        result['owned_scratch_removed'] = not scratch.exists()
        (output / 'validation.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    raise SystemExit(main())
