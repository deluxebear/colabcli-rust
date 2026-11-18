#!/usr/bin/env python3
"""Compare explicit-session URLs with the pinned merged CLI using local synthetic state."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.parse import quote
from capture_merged_help import fingerprint_sources, normalize
from compare_merged_accounts import fixture, load_app
from compare_merged_account_mutations import require_no_native_keyring
from compare_merged_drive_credentials import snapshot

ROOT = Path(__file__).resolve().parents[1]


def child(args):
    app, violations = load_app(args)
    import importlib.metadata
    from typer.testing import CliRunner
    result = CliRunner().invoke(app, json.loads(args.arguments), prog_name='colab', color=False)
    print(json.dumps({'exit_code': result.exit_code, 'stdout': result.stdout, 'stderr': result.stderr,
                      'dependency_versions': {name: importlib.metadata.version(name) for name in ('typer', 'click', 'rich')},
                      'python_version': sys.version.split()[0],
                      'forbidden_operations': violations}))


def setup(home, name):
    fixture(home, name)
    root = home / 'config'
    registry = json.loads((root / 'accounts.json').read_text())
    emails = {'default': 'primary@example.invalid', 'work': 'compute+雪@example.invalid', 'adc': 'drive@example.invalid'}
    if name == 'no-compute-email':
        emails['work'] = None
    if name == 'empty-compute-email':
        emails['work'] = ''
    for profile, email in emails.items():
        registry['accounts'][profile]['email'] = email
    (root / 'accounts.json').write_text(json.dumps(registry))
    names = {'default': ['shared'], 'work': ['unique', 'shared', 'duplicate'], 'adc': ['duplicate']}
    for profile, items in names.items():
        sessions = {key: {'name': key, 'endpoint': 'ep-' + profile + '-' + key,
                          'token': 'synthetic-proxy-never-output', 'url': 'https://runtime.invalid'} for key in items}
        if name == 'opaque-endpoint' and profile == 'work':
            sessions['unique']['endpoint'] = 'ep/雪 #?%'
        (root / 'accounts' / profile / 'sessions.json').write_text(json.dumps(sessions))
    return emails


def native_url(host, endpoint, email):
    backend = '/tun/m/' + quote(endpoint, safe='')
    authuser = 'authuser=' + quote(email, safe='') + '&' if email else ''
    return host + '/notebooks/empty.ipynb?' + authuser + 'dbu=' + quote(backend, safe='') + '#datalabBackendUrl=' + host + backend + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/no-keyring/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-url-comparison.json')
    parser.add_argument('--child', action='store_true')
    parser.add_argument('--arguments')
    args = parser.parse_args()
    if args.child:
        return child(args)
    if not args.python:
        parser.error('--python required')
    require_no_native_keyring(args.rust)
    sources = fingerprint_sources({key: getattr(args, key) for key in ('cli', 'ext', 'kernel')})
    args.deps = Path(subprocess.check_output([str(args.python), '-S', '-c',
                                            'import sysconfig;print(sysconfig.get_path("purelib"))'], text=True).strip())
    cases = [
        ('active-owner', ['url', '-s', 'shared'], {}, 'default'),
        ('unique-owner', ['url', '-s', 'unique'], {}, 'work'),
        ('account-root', ['-a', 'work', 'url', '-s', 'shared'], {}, 'work'),
        ('account-local', ['url', '-s', 'shared', '-a', 'work'], {}, 'work'),
        ('account-env', ['url', '-s', 'shared'], {'COLAB_ACCOUNT': 'work'}, 'work'),
        ('flag-over-env', ['-a', 'default', 'url', '-s', 'shared'], {'COLAB_ACCOUNT': 'work'}, 'default'),
        ('email-selector', ['-a', 'COMPUTE+雪@example.invalid', 'url', '-s', 'shared'], {}, 'work'),
        ('drive-env-ignored', ['-a', 'work', 'url', '-s', 'shared'], {'COLAB_DRIVE_ACCOUNT': 'adc'}, 'work'),
        ('no-compute-email', ['-a', 'work', 'url', '-s', 'shared'], {'COLAB_DRIVE_ACCOUNT': 'adc'}, 'work'),
        ('empty-compute-email', ['-a', 'work', 'url', '-s', 'shared'], {'COLAB_DRIVE_ACCOUNT': 'adc'}, 'work'),
        ('sandbox-origin', ['-a', 'work', 'url', '-s', 'unique', '--host', 'https://sandbox.example.invalid/'], {}, 'work'),
        ('normalized-origin', ['-a', 'work', 'url', '-s', 'unique', '--host', 'https://EXAMPLE.invalid:443/'], {}, 'work'),
        ('opaque-endpoint', ['-a', 'work', 'url', '-s', 'unique'], {}, 'work'),
        ('explicit-owner-missing-session', ['-a', 'default', 'url', '-s', 'unique'], {}, None),
        ('ambiguous-owner', ['url', '-s', 'duplicate'], {}, None),
        ('missing-account', ['-a', 'missing', 'url', '-s', 'shared'], {}, None),
    ]
    rows, runtime = [], None
    for name, arguments, extra, owner in cases:
        row = {'name': name, 'arguments': arguments, 'expected_owner': owner}
        for implementation in ('official', 'rust'):
            with tempfile.TemporaryDirectory(prefix='merged-url-') as directory:
                home = Path(directory).resolve()
                emails = setup(home, name)
                before = snapshot(home)
                env = dict(HOME=str(home), PATH=os.defpath, LANG='C.UTF-8', COLAB_TOKEN_STORAGE='file',
                           COLAB_CLI_CONFIG_DIR=str(home / 'config'), NO_COLOR='1', TERM='dumb', **extra)
                if implementation == 'official':
                    command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps),
                               '--arguments', json.dumps(arguments)]
                    for key in ('cli', 'ext', 'kernel'):
                        command += ['--' + key, str(getattr(args, key).resolve())]
                else:
                    command = [str(args.rust.resolve()), '--token-storage', 'file', *arguments]
                run = subprocess.run(command, env=env, cwd=home, capture_output=True, text=True, timeout=20)
                if implementation == 'official':
                    assert run.returncode == 0, run.stderr
                    result = json.loads(run.stdout)
                    assert not result.pop('forbidden_operations'), name
                    observed_runtime = {key: result.pop(key) for key in ('dependency_versions', 'python_version')}
                    assert runtime is None or observed_runtime == runtime
                    runtime = observed_runtime
                else:
                    result = {'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr}
                expected_code = 2 if name == 'account-local' and implementation == 'official' else (0 if owner else 1)
                assert result['exit_code'] == expected_code, (name, implementation, result)
                assert snapshot(home) == before, (name, implementation, 'payload changed')
                assert not any(secret in result['stdout'] + result['stderr'] for secret in
                               ('synthetic-proxy-never-output', 'fixture-never-output', 'fixture-proxy-never-output'))
                if owner and implementation == 'rust':
                    host = 'https://colab.research.google.com'
                    if name == 'sandbox-origin': host = 'https://sandbox.example.invalid'
                    if name == 'normalized-origin': host = 'https://example.invalid'
                    session = arguments[arguments.index('-s') + 1]
                    endpoint = 'ep/雪 #?%' if name == 'opaque-endpoint' else 'ep-' + owner + '-' + session
                    assert result['stdout'] == native_url(host, endpoint, emails[owner]), (name, 'native URL contract')
                row[implementation] = {key: normalize(value, home) if isinstance(value, str) else value for key, value in result.items()}
        if name == 'unique-owner':
            assert row['official']['stderr'] == "[colab] Routing session 'unique' -> account 'work'.\n"
        if name == 'ambiguous-owner':
            assert row['official']['exit_code'] == 1 and not row['official']['stdout']
            assert 'exists in multiple accounts (work, adc)' in row['official']['stderr']
        if row['official'] == row['rust']:
            row['comparison'] = 'matched'
        elif name == 'account-local':
            assert 'No such option: -a' in row['official']['stderr'] and row['official']['stdout'] == ''
            assert row['rust']['stderr'] == ''
            row['comparison'] = 'global_option_extension'
        elif name in ('normalized-origin', 'opaque-endpoint'):
            assert row['official']['stderr'] == row['rust']['stderr'] == ''
            email = 'compute+雪@example.invalid'
            if name == 'normalized-origin':
                assert row['official']['stdout'] == native_url('https://EXAMPLE.invalid:443', 'ep-work-unique', email)
            else:
                host = 'https://colab.research.google.com'
                backend = '/tun/m/ep/雪 #?%'
                assert row['official']['stdout'] == host + '/notebooks/empty.ipynb?authuser=' + quote(email, safe='') + '&dbu=' + quote(backend, safe='') + '#datalabBackendUrl=' + host + backend + '\n'
            row['comparison'] = 'origin_normalization' if name == 'normalized-origin' else 'opaque_endpoint_encoding'
        elif owner:
            raise AssertionError((name, 'unexpected URL output difference', row))
        else:
            assert row['official']['stdout'] == row['rust']['stdout'] == ''
            official_marker, native_marker = {
                'missing-account': ("Account 'missing' not found", '找不到账户: missing'),
            }[name]
            assert official_marker in row['official']['stderr'] and native_marker in row['rust']['stderr']
            row['comparison'] = 'error_diagnostic'
        rows.append(row)
    summary = {'cases': len(rows)}
    for row in rows:
        kind = row['comparison']
        summary[kind] = summary.get(kind, 0) + 1
    args.output.write_text(json.dumps({'sources': sources,
        'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(), 'rust_native_keyring_compiled': False,
        'scope': 'Actual merged explicit-session URL commands; synthetic local profiles; no browser/network/keyring allowed',
        'normalization': ['temporary HOME', 'ANSI SGR display sequences'], 'official_runtime': runtime, 'cases': rows, 'summary': summary,
        'not_checked': ['implicit session sync', 'browser launch', 'live VM/frontend connection']}, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(summary))


if __name__ == '__main__':
    main()
