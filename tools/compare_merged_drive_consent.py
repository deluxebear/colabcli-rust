#!/usr/bin/env python3
"""Compare offline consent profile registration and URL contracts with the fixed merged CLI."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.parse import parse_qs, urlsplit
from capture_merged_help import fingerprint_sources
from compare_merged_accounts import fixture, load_app
from compare_merged_account_mutations import require_no_native_keyring
from compare_merged_drive_credentials import snapshot

ROOT = Path(__file__).resolve().parents[1]
OPTIONS = ['--url-only', '--no-browser', '--flavor', 'custom', '--client-id', 'synthetic-consent-client',
           '--client-secret', 'synthetic-consent-secret', '--redirect-uri', 'https://example.invalid/callback',
           '--scopes', 'openid https://www.googleapis.com/auth/drive']


def child(args):
    app, violations = load_app(args)
    import importlib.metadata
    from typer.testing import CliRunner
    result = CliRunner().invoke(app, json.loads(args.arguments), prog_name='colab', color=False)
    print(json.dumps({'exit_code': result.exit_code, 'stdout': result.stdout, 'stderr': result.stderr,
                      'dependency_versions': {name: importlib.metadata.version(name) for name in
                                              ('google-auth-oauthlib', 'oauthlib', 'requests-oauthlib')},
                      'python_version': sys.version.split()[0],
                      'forbidden_operations': violations}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/no-keyring/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-drive-consent.json')
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
        ('root-account', ['-a', 'fresh', 'drive', 'consent'], {}, 'fresh', ['fresh']),
        ('group-account', ['drive', '-a', 'fresh', 'consent'], {}, 'fresh', ['fresh']),
        ('command-account', ['dfs', 'consent', '-a', 'fresh'], {}, 'fresh', ['fresh']),
        ('compute-env', ['drive', 'consent'], {'COLAB_ACCOUNT': 'fresh'}, 'fresh', ['fresh']),
        ('drive-env', ['drive', 'consent'], {'COLAB_DRIVE_ACCOUNT': 'storage'}, 'storage', ['storage']),
        ('legacy-drive-env', ['drive', 'consent'], {'COLAB_DFS_ACCOUNT': 'storage'}, 'storage', ['storage']),
        ('two-new-profiles', ['-a', 'fresh', 'drive', 'consent'], {'COLAB_DRIVE_ACCOUNT': 'storage'}, 'storage', ['fresh', 'storage']),
        ('consent-ignores-binding', ['-a', 'work', 'drive', 'consent'], {}, 'work', []),
        ('existing-email', ['-a', 'FIXTURE@example.invalid', 'drive', 'consent'], {}, 'work', []),
    ]
    rows, runtime = [], None
    for name, arguments, extra, owner, added in cases:
        row = {'name': name, 'arguments': arguments, 'pending_owner': owner, 'new_profiles': added}
        for implementation in ('official', 'rust'):
            with tempfile.TemporaryDirectory(prefix='merged-drive-consent-') as directory:
                home = Path(directory).resolve()
                fixture(home, name)
                registry_path = home / 'config/accounts.json'
                before_registry = json.loads(registry_path.read_text())
                for profile, metadata in before_registry['accounts'].items():
                    before_registry['accounts'][profile] = dict(
                        email=None, user_id=None, dfs_email=None, auth_provider='oauth2',
                        adc_credentials_file=None, drive_account=None, migrated_legacy=False,
                        created_at='2026-01-01T00:00:00Z', updated_at='2026-01-01T00:00:00Z') | metadata
                registry_path.write_text(json.dumps(before_registry))
                before = snapshot(home)
                env = dict(HOME=str(home), PATH=os.defpath, LANG='C.UTF-8', COLAB_TOKEN_STORAGE='file',
                           COLAB_CLI_CONFIG_DIR=str(home / 'config'), NO_COLOR='1', TERM='dumb', **extra)
                if implementation == 'official':
                    command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps),
                               '--arguments', json.dumps(arguments + OPTIONS)]
                    for key in ('cli', 'ext', 'kernel'):
                        command += ['--' + key, str(getattr(args, key).resolve())]
                else:
                    command = [str(args.rust.resolve()), '--token-storage', 'file', *arguments, *OPTIONS]
                result = subprocess.run(command, env=env, cwd=home, capture_output=True, text=True, timeout=20)
                if implementation == 'official':
                    assert result.returncode == 0, result.stderr
                    result = json.loads(result.stdout)
                    assert not result.pop('forbidden_operations')
                    observed_runtime = {key: result.pop(key) for key in ('dependency_versions', 'python_version')}
                    assert runtime is None or observed_runtime == runtime
                    runtime = observed_runtime
                else:
                    result = {'exit_code': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
                assert result['exit_code'] == 0, (name, implementation, result['stderr'])
                assert 'synthetic-consent-secret' not in result['stdout'] + result['stderr']
                url = urlsplit(result['stdout'].strip())
                assert url.scheme == 'https' and url.netloc == 'accounts.google.com'
                query = parse_qs(url.query)
                for field, expected in {'client_id': 'synthetic-consent-client', 'response_type': 'code',
                                        'redirect_uri': 'https://example.invalid/callback', 'access_type': 'offline',
                                        'scope': 'openid https://www.googleapis.com/auth/drive'}.items():
                    assert query[field] == [expected], (name, implementation, field)
                expected_hint = 'fixture@example.invalid' if owner == 'work' else None
                assert query.get('login_hint') == ([expected_hint] if expected_hint else None), (name, implementation, 'login hint')
                pending_path = home / 'config/accounts' / owner / 'dfs_pending.json'
                pending = json.loads(pending_path.read_text())
                assert pending['account'] == owner and pending['state'] == query['state'][0]
                verifier = pending.get('code_verifier')
                pkce = bool(verifier and query.get('code_challenge_method') == ['S256'] and
                            query.get('code_challenge') == [base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b'=').decode()])
                if implementation == 'rust':
                    assert pkce, 'native consent must preserve S256 PKCE'
                registry = json.loads(registry_path.read_text())
                assert list(registry['accounts']) == list(before_registry['accounts']) + added
                assert registry['active'] == before_registry['active'] and registry.get('default_drive_account') == before_registry.get('default_drive_account')
                assert all(registry['accounts'][key] == value for key, value in before_registry['accounts'].items())
                allowed = {'config/accounts.json', 'config/accounts/' + owner + '/dfs_pending.json'}
                after = snapshot(home)
                assert all(before.get(path) == after.get(path) for path in set(before) | set(after) if path not in allowed), 'unexpected credential or session mutation'
                row[implementation] = {'exit_code': 0, 'account_order': list(registry['accounts']),
                                       'active': registry['active'], 'pending_account': pending['account'],
                                       'url_parameters_verified': True, 'login_hint': expected_hint,
                                       'state_matches_pending': True, 's256_pkce_verified': pkce,
                                       'other_payloads_unchanged': True}
        rows.append(row)
    args.output.write_text(json.dumps({'sources': sources,
        'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(), 'rust_native_keyring_compiled': False,
        'scope': 'Actual offline merged consent URL and profile routing; no browser, network or real credentials; no token exchange',
        'cases': rows, 'summary': {'cases': len(rows), 'routing_contracts_passed': len(rows)},
        'official_runtime': runtime,
        'differences': ['Pending schemas and progress text differ'],
        'not_checked': ['Google authorization', 'authorization code exchange', 'Drive mount']}, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'cases': len(rows), 'routing_contracts_passed': len(rows)}))


if __name__ == '__main__':
    main()
