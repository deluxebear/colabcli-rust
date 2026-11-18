#!/usr/bin/env python3
"""Exercise actual merged account mutations in synthetic isolated configurations."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from capture_merged_help import fingerprint_sources
from compare_merged_accounts import fixture, load_app, output_record

ROOT = Path(__file__).resolve().parents[1]


def scenarios():
    return [
        ('use-name', [['account', 'use', 'work']]),
        ('use-email', [['acct', 'switch', 'FIXTURE@example.invalid']]),
        ('use-empty-selector', [['account', 'use', '']]),
        ('use-missing', [['account', 'use', 'missing']]),
        ('bind', [['account', 'set-drive', 'adc', '-a', 'work']]),
        ('bind-current', [['--account', 'work', 'account', 'set-drive', 'adc']]),
        ('bind-clear', [['account', 'set-drive', '--clear', '-a', 'work']]),
        ('bind-clear-with-ignored-selector', [['account', 'set-drive', 'missing', '--clear', '-a', 'work']]),
        ('default-drive', [['account', 'set-drive', 'adc', '--default']]),
        ('clear-default-drive', [['account', 'set-drive', '--default', '--clear']]),
        ('missing-drive-argument', [['account', 'set-drive']]),
        ('missing-drive-selector', [['account', 'set-drive', 'missing']]),
        ('missing-compute-selector', [['account', 'set-drive', 'adc', '-a', 'missing']]),
        ('rename', [['account', 'rename', 'work', 'new-work']]),
        ('rename-email', [['account', 'rename', 'FIXTURE@example.invalid', 'new-work']]),
        ('rename-active-and-bindings', [['account', 'use', 'default'], ['account', 'rename', 'default', 'new-default']]),
        ('rename-same', [['account', 'rename', 'work', 'work']]),
        ('rename-missing', [['account', 'rename', 'missing', 'new-work']]),
        ('rename-collision', [['account', 'rename', 'work', 'adc']]),
        ('rename-invalid', [['account', 'rename', 'work', '../escape']]),
        ('remove-empty', [['account', 'remove', 'adc']]),
        ('remove-sessions-refused', [['account', 'remove', 'work']]),
        ('remove-sessions-forced', [['acct', 'rm', 'work', '--force']]),
        ('remove-email', [['account', 'remove', 'FIXTURE@example.invalid', '--force']]),
        ('remove-missing', [['account', 'remove', 'missing']]),
        ('remove-active-preserves-order', [['account', 'use', 'default'], ['account', 'remove', 'default']]),
        ('remove-last-recreates-default', [['account', 'remove', 'default'], ['account', 'remove', 'work', '--force'], ['account', 'remove', 'adc']]),
    ]


def payload_snapshot(home):
    return {str(path.relative_to(home)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (home / 'config').rglob('*') if path.is_file() and not path.name.endswith('.lock')}


def snapshot(home):
    root = home / 'config'
    registry = json.loads((root / 'accounts.json').read_text())
    # Compare business metadata; Rust materializes optional defaults and timestamps.
    projected = []
    for name, meta in registry['accounts'].items():
        projected.append({'account': name, 'email': meta.get('email'), 'user_id': meta.get('user_id'),
            'auth_provider': meta.get('auth_provider', 'oauth2'), 'drive_account': meta.get('drive_account'),
            'adc_credentials_file': meta.get('adc_credentials_file'), 'migrated_legacy': meta.get('migrated_legacy', False)})
    files = {}
    for path in sorted((root / 'accounts').rglob('*')):
        if not path.is_file() or path.name.endswith('.lock'):
            continue
        data = path.read_bytes()
        try:
            data = json.dumps(json.loads(data), sort_keys=True, separators=(',', ':')).encode()
        except (ValueError, UnicodeDecodeError):
            pass
        files[str(path.relative_to(root))] = hashlib.sha256(data).hexdigest()
    return {'active': registry['active'], 'default_drive_account': registry.get('default_drive_account'), 'accounts': projected, 'files_sha256': files,
            'pending_account_transaction': (root / 'account-transaction.json').exists()}


def child(args):
    app, violations = load_app(args)
    from typer.testing import CliRunner
    steps = json.loads(args.steps)
    results = []
    home = Path(os.environ['HOME'])
    for arguments in steps:
        before = payload_snapshot(home)
        result = CliRunner().invoke(app, arguments, prog_name='colab', color=False)
        unchanged = before == payload_snapshot(home) if result.exit_code else None
        results.append({'exit_code': result.exit_code, 'stdout': result.stdout, 'stderr': result.stderr, 'state': snapshot(home), 'failure_payload_unchanged': unchanged})
    print(json.dumps({'results': results, 'forbidden_operations': violations}))


def require_no_native_keyring(binary):
    capabilities = subprocess.run([str(binary.resolve()), 'ext', 'version', '--json'], env={'PATH': os.defpath}, capture_output=True, text=True, check=True, timeout=10)
    if json.loads(capabilities.stdout).get('native_keyring_compiled') is not False:
        raise SystemExit('mutation probe requires a --no-default-features binary; native or unknown keyring builds are forbidden')


def main(case_provider=scenarios, extra_environment=None, default_output=None, scope=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/no-keyring/colab')
    parser.add_argument('--output', type=Path, default=default_output or ROOT / 'research/merged-account-mutations.json')
    parser.add_argument('--child', action='store_true')
    parser.add_argument('--steps')
    args = parser.parse_args()
    if args.child:
        return child(args)
    if not args.python:
        parser.error('--python is required')
    require_no_native_keyring(args.rust)
    sources = fingerprint_sources({key: getattr(args, key) for key in ('cli', 'ext', 'kernel')})
    args.deps = Path(subprocess.check_output([str(args.python), '-S', '-c', 'import sysconfig; print(sysconfig.get_path("purelib"))'], text=True).strip())
    rows = []
    for name, steps in case_provider():
        row = {'name': name, 'steps': steps}
        for implementation in ('official', 'rust'):
            with tempfile.TemporaryDirectory(prefix='merged-account-mutation-') as directory:
                home = Path(directory).resolve()
                fixture(home, name)
                if name == 'use-empty-selector':
                    registry_file = home / 'config/accounts.json'
                    registry = json.loads(registry_file.read_text())
                    registry['accounts']['work']['email'] = ''
                    registry_file.write_text(json.dumps(registry))
                # Complete synthetic OAuth shape allows both implementations to migrate the same token.
                token = {'token': 'fixture-access', 'refresh_token': 'fixture-refresh', 'token_uri': 'https://oauth2.googleapis.com/token', 'client_id': 'fixture-client', 'client_secret': 'fixture-secret', 'scopes': [], 'expiry': None}
                (home / 'config/accounts/work/oauth_token.json').write_text(json.dumps(token))
                env = {'HOME': str(home), 'PATH': os.defpath, 'LANG': 'C.UTF-8', 'COLAB_TOKEN_STORAGE': 'file', 'COLAB_CLI_CONFIG_DIR': str(home / 'config'), 'NO_COLOR': '1', 'TERM': 'dumb'}
                if extra_environment:
                    env.update(extra_environment(name))
                if implementation == 'official':
                    command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps), '--steps', json.dumps(steps)]
                    for key in ('cli', 'ext', 'kernel'):
                        command += ['--' + key, str(getattr(args, key).resolve())]
                    run = subprocess.run(command, env=env, cwd=home, capture_output=True, text=True, timeout=20)
                    if run.returncode:
                        raise RuntimeError(run.stderr)
                    result = json.loads(run.stdout)
                    row['forbidden_operations'] = result['forbidden_operations']
                    results = result['results']
                else:
                    results = []
                    for arguments in steps:
                        before = payload_snapshot(home)
                        run = subprocess.run([str(args.rust.resolve()), *arguments], env=env, cwd=home, capture_output=True, text=True, timeout=15)
                        unchanged = before == payload_snapshot(home) if run.returncode else None
                        results.append({'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr, 'state': snapshot(home), 'failure_payload_unchanged': unchanged})
                records = []
                for result, arguments in zip(results, steps):
                    assert all(secret not in result['stdout'] + result['stderr'] for secret in ('fixture-access', 'fixture-refresh', 'fixture-secret', 'fixture-proxy-never-output'))
                    record = output_record(result, home, arguments)
                    record['state'] = result['state']
                    record['failure_payload_unchanged'] = result['failure_payload_unchanged']
                    records.append(record)
                row[implementation] = records
        row['matched'] = row['official'] == row['rust'] and all(step['failure_payload_unchanged'] is not False for key in ('official', 'rust') for step in row[key])
        rows.append(row)
    report = {'sources': sources, 'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(),
        'rust_native_keyring_compiled': False,
        'scope': scope or 'merged account use/set-drive/rename/remove on temporary synthetic file credentials; no cloud resources',
        'normalization': ['temporary HOME', 'JSON object field order and whitespace for file hashes', 'optional account defaults and nondeterministic timestamps'],
        'cases': rows, 'summary': {'cases': len(rows), 'matched': sum(row['matched'] for row in rows)}}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(report['summary']))
    if any(not row['matched'] or row['forbidden_operations'] for row in rows):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
