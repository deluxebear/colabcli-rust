#!/usr/bin/env python3
"""Compare offline account projections with the pinned merged CLI, in isolated homes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from capture_merged_help import fingerprint_sources, normalize

ROOT = Path(__file__).resolve().parents[1]


def scenarios():
    return [
        ('list-json', ['account', 'list', '--json'], {}),
        ('list-text', ['account', 'list'], {}),
        ('list-alias', ['acct', 'ls', '--json'], {}),
        ('current-json', ['account', 'current', '--json'], {}),
        ('current-text', ['account', 'current'], {}),
        ('current-flag', ['--account', 'work', 'account', 'current', '--json'], {}),
        ('current-env', ['account', 'current', '--json'], {'COLAB_ACCOUNT': 'work'}),
        ('flag-before-env', ['--account', 'default', 'account', 'current', '--json'], {'COLAB_ACCOUNT': 'work'}),
        ('drive-env', ['account', 'list', '--json'], {'COLAB_DRIVE_ACCOUNT': 'default'}),
        ('drive-legacy-env', ['account', 'current', '--json'], {'COLAB_DFS_ACCOUNT': 'work'}),
        ('adc-gcloud', ['--account', 'adc', 'account', 'current', '--json'], {}),
        ('adc-configured-file', ['--account', 'adc', 'account', 'current', '--json'], {'GOOGLE_APPLICATION_CREDENTIALS': '<HOME>/missing.json'}),
        ('adc-configured-missing-no-fallback', ['--account', 'adc', 'account', 'current', '--json'], {}),
        ('broken-oauth-file', ['--account', 'work', 'account', 'current', '--json'], {}),
        ('adc-missing', ['--account', 'adc', 'account', 'current', '--json'], {}),
        ('adc-file', ['--account', 'adc', 'account', 'current', '--json'], {'GOOGLE_APPLICATION_CREDENTIALS': '<HOME>/adc.json'}),
    ]


def fixture(home, scenario):
    root = home / 'config'
    accounts = {'default': {}, 'work': {'email': 'fixture@example.invalid', 'user_id': 'fixture-user', 'drive_account': 'default'}, 'adc': {'auth_provider': 'adc'}}
    if scenario == 'adc-configured-file':
        accounts['adc']['adc_credentials_file'] = '~/adc.json'
    if scenario == 'adc-configured-missing-no-fallback':
        accounts['adc']['adc_credentials_file'] = '~/missing.json'
    if scenario in ('adc-gcloud', 'adc-configured-missing-no-fallback'):
        path = home / '.config/gcloud/application_default_credentials.json'
        path.parent.mkdir(parents=True)
        path.write_text('not-parsed')
    root.mkdir(parents=True)
    (root / 'accounts.json').write_text(json.dumps({'version': 1, 'active': 'default', 'default_drive_account': 'work', 'accounts': accounts}))
    for name in accounts:
        path = root / 'accounts' / name
        (path / 'history').mkdir(parents=True)
        (path / 'sessions.json').write_text(json.dumps({key: {'name': key, 'endpoint': 'fixture-endpoint', 'token': 'fixture-proxy-never-output', 'url': 'https://runtime.invalid'} for key in (['z-last', 'a-first'] if name == 'work' else [])}))
    (root / 'accounts/work/oauth_token.json').write_text('{"refresh_token":"fixture-never-output"}')
    (root / 'accounts/default/dfs_credentials.json').write_text('{}')
    (home / 'adc.json').write_text('{}')
    if scenario == 'broken-oauth-file':
        (root / 'accounts/work/oauth_token.json').write_text('invalid-json')
    for path in home.rglob('*'):
        if path.is_file():
            path.chmod(0o600)


def loopback_endpoint(value):
    from urllib.parse import urlsplit
    origin = urlsplit(value)
    if origin.scheme != 'http' or origin.hostname != '127.0.0.1' or not origin.port or origin.path or origin.query or origin.fragment or origin.username is not None or origin.password is not None:
        raise ValueError('probe requires an exact IPv4 loopback HTTP origin')
    return origin.hostname, origin.port


def loopback_address(endpoint, address):
    return isinstance(address, tuple) and len(address) == 2 and address == endpoint


def load_app(args, loopback_origin=None):
    sys.path[:0] = [str(args.cli / 'src'), str(args.kernel), str(args.ext), str(args.deps)]
    import socket
    import webbrowser
    violations = []

    def forbidden(*_args, **_kwargs):
        violations.append('forbidden external operation')
        raise RuntimeError('offline merged account probe forbids external operations')

    if loopback_origin is None:
        socket.getaddrinfo = socket.socket.connect = socket.socket.connect_ex = forbidden
        socket.create_connection = forbidden
    else:
        endpoint = loopback_endpoint(loopback_origin)
        original_dns, original_connect, original_connect_ex = socket.getaddrinfo, socket.socket.connect, socket.socket.connect_ex
        def check(address):
            if not loopback_address(endpoint, address):
                forbidden()
        def resolve(host, port, *values, **kwargs):
            check((host, port))
            return original_dns(host, port, *values, **kwargs)
        def connect(sock, address):
            check(address)
            return original_connect(sock, address)
        def connect_ex(sock, address):
            check(address)
            return original_connect_ex(sock, address)
        socket.getaddrinfo, socket.socket.connect, socket.socket.connect_ex = resolve, connect, connect_ex
    webbrowser.open = webbrowser.open_new = webbrowser.open_new_tab = forbidden
    subprocess.Popen = subprocess.run = subprocess.call = subprocess.check_call = subprocess.check_output = forbidden
    os.system = os.popen = forbidden
    import keyring
    keyring.get_password = keyring.set_password = keyring.delete_password = forbidden
    from colab_cli import auto_update
    auto_update.run_background_check = lambda: None
    from colab_cli.cli import app
    import colab_ext
    import colab_cli.cli
    import jupyter_kernel_client
    for module, root in [(colab_ext, args.ext), (colab_cli.cli, args.cli), (jupyter_kernel_client, args.kernel)]:
        assert Path(module.__file__).resolve().is_relative_to(root.resolve())
    assert colab_ext.register(app)
    return app, violations


def child(args):
    app, violations = load_app(args)
    from typer.testing import CliRunner
    result = CliRunner().invoke(app, args.arguments, prog_name='colab', color=False)
    print(json.dumps({'exit_code': result.exit_code, 'stdout': result.stdout, 'stderr': result.stderr, 'forbidden_operations': violations}))


def output_record(result, home, arguments):
    record = {key: normalize(result[key], home) if isinstance(result[key], str) else result[key] for key in ('exit_code', 'stdout', 'stderr')}
    if '--json' in arguments and record['exit_code'] == 0:
        record['json'] = json.loads(record.pop('stdout'))
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/debug/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-account-comparison.json')
    parser.add_argument('--child', action='store_true')
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.child:
        args.arguments = args.arguments[1:]
        return child(args)
    if not args.python:
        parser.error('--python is required')
    sources = fingerprint_sources({key: getattr(args, key) for key in ('cli', 'ext', 'kernel')})
    args.deps = Path(subprocess.check_output([str(args.python), '-S', '-c', 'import sysconfig; print(sysconfig.get_path("purelib"))'], text=True).strip())
    rows = []
    for name, arguments, extra in scenarios():
        row = {'name': name, 'arguments': arguments}
        for implementation in ('official', 'rust'):
            with tempfile.TemporaryDirectory(prefix='merged-account-') as directory:
                home = Path(directory).resolve()
                fixture(home, name)
                env = {'HOME': str(home), 'PATH': os.environ.get('PATH', ''), 'LANG': 'C.UTF-8', 'COLAB_TOKEN_STORAGE': 'file',
                       'COLAB_CLI_CONFIG_DIR': str(home / 'config'), 'NO_COLOR': '1', 'TERM': 'dumb'}
                env.update({key: value.replace('<HOME>', str(home)) for key, value in extra.items()})
                before = {str(path.relative_to(home)): path.read_bytes() for path in home.rglob('*') if path.is_file()}
                if implementation == 'official':
                    command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps)]
                    for key in ('cli', 'ext', 'kernel'):
                        command += ['--' + key, str(getattr(args, key).resolve())]
                    command += ['--', *arguments]
                else:
                    command = [str(args.rust.resolve()), *arguments]
                run = subprocess.run(command, capture_output=True, text=True, cwd=home, env=env, timeout=15)
                if implementation == 'official':
                    if run.returncode:
                        raise RuntimeError(run.stderr)
                    result = json.loads(run.stdout)
                    row['forbidden_operations'] = result['forbidden_operations']
                else:
                    result = {'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr}
                row[implementation] = output_record(result, home, arguments)
                # Bootstrap may add lock files/directories; existing payload files must remain intact.
                row[implementation + '_payload_unchanged'] = all((home / path).read_bytes() == contents for path, contents in before.items())
                assert all(secret not in result['stdout'] + result['stderr'] for secret in ('fixture-never-output', 'fixture-proxy-never-output'))
        row['matched'] = row['official'] == row['rust'] and row['official']['exit_code'] == 0
        rows.append(row)
    report = {'sources': sources, 'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(), 'scope': 'offline merged account list/current with file storage; no real credentials or cloud resources', 'cases': rows,
              'summary': {'cases': len(rows), 'matched': sum(row['matched'] for row in rows)}}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(report['summary']))
    if any(not row['matched'] or row['forbidden_operations'] or not row['official_payload_unchanged'] or not row['rust_payload_unchanged'] for row in rows):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
