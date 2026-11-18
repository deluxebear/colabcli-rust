#!/usr/bin/env python3
"""Compare merged Drive credential metadata/routing/clear with synthetic files only."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from capture_merged_help import fingerprint_sources, normalize
from compare_merged_accounts import fixture, load_app
from compare_merged_account_mutations import require_no_native_keyring

ROOT = Path(__file__).resolve().parents[1]


def scenarios():
    return [
        ('show-own', ['drive', 'show'], {}, 'default'),
        ('show-alias', ['dfs', 'show'], {}, 'default'),
        ('show-bound', ['-a', 'work', 'drive', 'show'], {}, 'default'),
        ('show-explicit', ['drive', 'show', '--drive-account', 'adc'], {}, 'adc'),
        ('show-env', ['drive', 'show'], {'COLAB_DRIVE_ACCOUNT': 'work'}, 'work'),
        ('show-legacy-env', ['drive', 'show'], {'COLAB_DFS_ACCOUNT': 'adc'}, 'adc'),
        ('show-global-fallback', ['drive', 'show'], {}, 'work'),
        ('show-empty', ['drive', 'show'], {}, 'work'),
        ('clear-own', ['drive', 'clear'], {}, 'default'),
        ('clear-env', ['drive', 'clear'], {'COLAB_DRIVE_ACCOUNT': 'work'}, 'work'),
        ('clear-missing', ['dfs', 'clear'], {}, 'default'),
        ('token-missing', ['drive', 'token', '--json'], {}, 'work'),
    ]


def setup(home, name):
    fixture(home, name)
    for account in ('default', 'work', 'adc'):
        path = home / 'config/accounts' / account / 'dfs_credentials.json'
        data = {'flavor': 'custom', 'user_id': 'synthetic-' + account,
                'email': account + '@example.invalid', 'created_at': '2026-01-01T00:00:00Z',
                'client_id': 'synthetic-client', 'client_secret': 'synthetic-client-secret',
                'refresh_token': 'synthetic-drive-refresh', 'token_uri': 'https://oauth2.googleapis.com/token',
                'scopes': ['openid', 'https://www.googleapis.com/auth/drive']}
        path.write_text(json.dumps(data))
        path.chmod(0o600)
    if name == 'show-global-fallback':
        (home / 'config/accounts/default/dfs_credentials.json').unlink()
    if name in ('show-empty', 'clear-missing', 'token-missing'):
        for path in (home / 'config').rglob('dfs_credentials.json'):
            path.unlink()


def snapshot(home):
    return {str(path.relative_to(home)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (home / 'config').rglob('*') if path.is_file() and not path.name.endswith('.lock')}


def child(args):
    app, violations = load_app(args)
    from typer.testing import CliRunner
    result = CliRunner().invoke(app, args.arguments[1:], prog_name='colab', color=False)
    print(json.dumps({'exit_code': result.exit_code, 'stdout': result.stdout,
                      'stderr': result.stderr, 'forbidden_operations': violations,
                      'exception_type': type(result.exception).__name__ if result.exception else None}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/no-keyring/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-drive-credentials.json')
    parser.add_argument('--child', action='store_true')
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.child:
        return child(args)
    if not args.python:
        parser.error('--python required')
    require_no_native_keyring(args.rust)
    sources = fingerprint_sources({key: getattr(args, key) for key in ('cli', 'ext', 'kernel')})
    args.deps = Path(subprocess.check_output([str(args.python), '-S', '-c',
                                            'import sysconfig;print(sysconfig.get_path("purelib"))'], text=True).strip())
    rows = []
    for name, arguments, extra, selected in scenarios():
        row = {'name': name, 'arguments': arguments, 'expected_drive_account': selected}
        for implementation in ('official', 'rust'):
            with tempfile.TemporaryDirectory(prefix='merged-drive-') as directory:
                home = Path(directory).resolve()
                setup(home, name)
                before = snapshot(home)
                env = dict(HOME=str(home), PATH=os.defpath, LANG='C.UTF-8', COLAB_TOKEN_STORAGE='file',
                           COLAB_CLI_CONFIG_DIR=str(home / 'config'), NO_COLOR='1', TERM='dumb', **extra)
                if implementation == 'official':
                    command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps)]
                    for key in ('cli', 'ext', 'kernel'):
                        command += ['--' + key, str(getattr(args, key).resolve())]
                    command += ['--', *arguments]
                else:
                    command = [str(args.rust.resolve()), *arguments]
                run = subprocess.run(command, env=env, cwd=home, capture_output=True, text=True, timeout=20)
                if implementation == 'official':
                    if run.returncode:
                        raise RuntimeError(run.stderr)
                    result = json.loads(run.stdout)
                    assert not result.pop('forbidden_operations'), 'forbidden official operation'
                    exception = result.pop('exception_type')
                    if exception:
                        row['official_exception_type'] = exception
                else:
                    result = {'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr}
                assert not any(secret in result['stdout'] + result['stderr'] for secret in
                               ('synthetic-client-secret', 'synthetic-drive-refresh', 'fixture-proxy-never-output', 'fixture-never-output'))
                result = {key: normalize(value, home) if isinstance(value, str) else value for key, value in result.items()}
                after = snapshot(home)
                expected = dict(before)
                if name.startswith('clear-'):
                    expected.pop('config/accounts/' + selected + '/dfs_credentials.json', None)
                assert after == expected, (name, implementation, 'unexpected file mutation')
                if result['stdout'].lstrip().startswith('{'):
                    result['json'] = json.loads(result.pop('stdout'))
                row[implementation] = result
        official, rust = row['official'], row['rust']
        if official == rust:
            row['comparison'] = 'matched'
        elif name == 'token-missing':
            assert official['exit_code'] == rust['exit_code'] == 1
            assert row['official_exception_type'] == 'ValueError'
            assert official['stdout'] == official['stderr'] == rust['stdout'] == ''
            assert '没有 Drive 凭据' in rust['stderr']
            row['comparison'] = 'missing_credentials_diagnostic'
        else:
            raise AssertionError(('unexplained merged Drive difference', row))
        rows.append(row)
    summary = {kind: sum(row['comparison'] == kind for row in rows) for kind in
               ('matched', 'missing_credentials_diagnostic')}
    summary['cases'] = len(rows)
    args.output.write_text(json.dumps({'sources': sources,
        'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(),
        'rust_native_keyring_compiled': False,
        'scope': 'Actual merged Drive show/clear and token-without-credentials; synthetic private homes, no network/keyring/browser/subprocess allowed in official commands',
        'normalization': ['temporary HOME', 'JSON key order and whitespace'],
        'cases': rows, 'summary': summary}, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(summary))


if __name__ == '__main__':
    main()
