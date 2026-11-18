#!/usr/bin/env python3
"""Actual merged Drive refresh/whoami against a single isolated loopback origin."""
import argparse
import copy
from datetime import datetime
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from urllib.parse import parse_qs
from capture_merged_help import fingerprint_sources, normalize
from compare_merged_accounts import load_app
from compare_merged_account_mutations import require_no_native_keyring
from compare_merged_drive_credentials import setup, snapshot

ROOT = Path(__file__).resolve().parents[1]
PROFILES = ('default', 'work', 'adc')


class Backend(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, selected, case):
        super().__init__(('127.0.0.1', 0), Handler)
        self.base = 'http://127.0.0.1:' + str(self.server_port)
        self.selected, self.case = selected, case
        self.trace, self.errors = [], []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, value):
        encoded = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_POST(self):
        try:
            assert self.path == '/token' and not self.headers.get('Authorization')
            length = int(self.headers.get('Content-Length', '0'))
            assert 0 < length < 8192
            fields = parse_qs(self.rfile.read(length).decode())
            selected = self.server.selected
            scope = fields.pop('scope', None)
            assert scope in (None, ['openid https://www.googleapis.com/auth/drive'])
            assert fields == {'grant_type': ['refresh_token'],
                              'refresh_token': ['synthetic-drive-refresh-' + selected],
                              'client_id': ['synthetic-client-' + selected],
                              'client_secret': ['synthetic-secret-' + selected]}
            self.server.trace.append({'method': 'POST', 'path': '/token',
                                      'drive_account': selected, 'scope_sent': scope is not None})
            if self.server.case == 'token-denied':
                self.reply(400, {'error': 'invalid_grant', 'error_description': 'synthetic-private-body'})
            else:
                self.reply(200, {'access_token': 'synthetic-access-' + selected,
                                 'refresh_token': 'synthetic-rotated-' + selected,
                                 'token_type': 'Bearer', 'expires_in': 5 if self.server.case == 'short-expiry' else 3600})
        except (AssertionError, ValueError, KeyError):
            self.server.errors.append('unexpected refresh request')
            self.reply(400, {'error': 'fixture-rejected'})

    def do_GET(self):
        selected = self.server.selected
        if self.path != '/userinfo' or self.headers.get('Authorization') != 'Bearer synthetic-access-' + selected:
            self.server.errors.append('unexpected userinfo request or identity')
            self.reply(400, {'error': 'fixture-rejected'})
            return
        self.server.trace.append({'method': 'GET', 'path': '/userinfo', 'drive_account': selected})
        if self.server.case == 'whoami-denied':
            self.reply(403, {'error': 'synthetic-private-body'})
        else:
            self.reply(200, {'sub': 'observed-' + selected, 'email': selected + '@observed.invalid'})


def child(args):
    app, violations = load_app(args, loopback_origin=args.base)
    import importlib.metadata
    import requests
    import urllib.request
    original_init, original_urlopen = requests.Session.__init__, urllib.request.urlopen

    def session_init(self, *values, **kwargs):
        original_init(self, *values, **kwargs)
        self.trust_env = False

    def urlopen(request, *values, **kwargs):
        # Redirect only the fixed userinfo constant; retain its actual headers and
        # implementation. Socket policy still permits only this exact origin.
        if isinstance(request, urllib.request.Request) and request.full_url == 'https://www.googleapis.com/oauth2/v3/userinfo':
            request = urllib.request.Request(args.base + '/userinfo', headers=dict(request.header_items()))
        return original_urlopen(request, *values, **kwargs)

    requests.Session.__init__ = session_init
    urllib.request.urlopen = urlopen
    urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))
    from typer.testing import CliRunner
    result = CliRunner().invoke(app, json.loads(args.arguments), prog_name='colab', color=False)
    print(json.dumps({'exit_code': result.exit_code, 'stdout': result.stdout, 'stderr': result.stderr,
                      'exception_type': type(result.exception).__name__ if result.exception else None,
                      'dependency_versions': {name: importlib.metadata.version(name) for name in
                                              ('google-auth', 'requests', 'typer')},
                      'python_version': sys.version.split()[0],
                      'forbidden_operations': violations}))


def state(home, selected):
    root = home / 'config/accounts' / selected
    value = json.loads((root / 'dfs_credentials.json').read_text())
    pending = root / 'dfs_refresh_pending.json'
    staged = json.loads(pending.read_text())['credentials'] if pending.exists() else None
    def project(data):
        return {'rotated_refresh_saved': data.get('refresh_token') == 'synthetic-rotated-' + selected,
                'fresh_access_saved': data.get('token') == 'synthetic-access-' + selected,
                'user_id': data.get('user_id'), 'email': data.get('email')}
    return {'credentials': project(value), 'refresh_pending': project(staged) if staged else None}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/no-keyring/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-drive-tokens.json')
    parser.add_argument('--child', action='store_true')
    parser.add_argument('--base')
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
        ('token-plain', ['-a', 'work', 'drive', 'token'], 'default'),
        ('token-json', ['drive', 'token', '--drive-account', 'adc', '--json'], 'adc'),
        ('whoami', ['-a', 'work', 'drive', 'token', '--whoami', '--json'], 'default'),
        ('short-expiry', ['drive', 'token', '--json'], 'default'),
        ('timezone-naive-expiry', ['drive', 'token', '--json'], 'default'),
        ('token-denied', ['drive', 'token', '--json'], 'default'),
        ('whoami-denied', ['drive', 'token', '--whoami', '--json'], 'default'),
    ]
    rows, runtime = [], None
    for name, arguments, selected in cases:
        zone = 'Asia/Shanghai' if name == 'timezone-naive-expiry' else 'UTC'
        row = {'name': name, 'arguments': arguments, 'drive_account': selected, 'timezone': zone}
        whoami, failed = '--whoami' in arguments, name.endswith('denied')
        for implementation in ('official', 'rust'):
            server = Backend(selected, name)
            thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.02}, daemon=True)
            thread.start()
            try:
                with tempfile.TemporaryDirectory(prefix='merged-drive-token-') as directory:
                    home = Path(directory).resolve()
                    setup(home, name)
                    for profile in PROFILES:
                        path = home / 'config/accounts' / profile / 'dfs_credentials.json'
                        value = json.loads(path.read_text())
                        value.update(token_uri=server.base + '/token', client_id='synthetic-client-' + profile,
                                     client_secret='synthetic-secret-' + profile, refresh_token='synthetic-drive-refresh-' + profile)
                        path.write_text(json.dumps(value))
                    before = snapshot(home)
                    registry_before = json.loads((home / 'config/accounts.json').read_text())
                    env = dict(HOME=str(home), PATH=os.defpath, LANG='C.UTF-8', COLAB_TOKEN_STORAGE='file',
                               COLAB_CLI_CONFIG_DIR=str(home / 'config'), NO_COLOR='1', TERM='dumb', TZ=zone)
                    if implementation == 'official':
                        command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps),
                                   '--base', server.base, '--arguments', json.dumps(arguments)]
                        for key in ('cli', 'ext', 'kernel'):
                            command += ['--' + key, str(getattr(args, key).resolve())]
                    else:
                        command = [str(args.rust.resolve()), '--token-storage', 'file', '--request-timeout', '2',
                                   '--drive-userinfo-url', server.base + '/userinfo', *arguments]
                    run = subprocess.run(command, env=env, cwd=home, capture_output=True, text=True, timeout=20)
                    if implementation == 'official':
                        assert run.returncode == 0, run.stderr
                        result = json.loads(run.stdout)
                        assert not result.pop('forbidden_operations')
                        observed_runtime = {key: result.pop(key) for key in ('dependency_versions', 'python_version')}
                        assert runtime is None or runtime == observed_runtime, 'dependency versions changed'
                        runtime = observed_runtime
                    else:
                        result = {'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr}
                    assert result['exit_code'] == (1 if failed else 0), (name, implementation, result)
                    for secret in ('synthetic-secret-', 'synthetic-drive-refresh-', 'synthetic-rotated-', 'synthetic-private-body', 'fixture-never-output'):
                        assert secret not in result['stdout'] + result['stderr'], 'unexpected secret output'
                    result = {key: normalize(value, home).replace(server.base, '<LOOPBACK>') if isinstance(value, str) else value
                              for key, value in result.items()}
                    if not failed:
                        value = json.loads(result.pop('stdout'))
                        expiry = value.pop('expires_in')
                        expected_expiry = (60, 60) if name in ('short-expiry', 'timezone-naive-expiry') and implementation == 'official' else ((0, 5) if name == 'short-expiry' else (3595, 3600))
                        assert expected_expiry[0] <= expiry <= expected_expiry[1], (name, implementation, expiry)
                        assert value == {'account': selected, 'access_token': 'synthetic-access-' + selected,
                                         'token_type': 'Bearer', 'scope': 'openid https://www.googleapis.com/auth/drive',
                                         'user_id': ('observed-' if whoami else 'synthetic-') + selected,
                                         'email': selected + ('@observed.invalid' if whoami else '@example.invalid')}
                        result['json'], result['expires_in'] = value, expiry
                    after = snapshot(home)
                    if name == 'token-denied':
                        assert after == before, 'failed refresh changed local payload'
                    allowed = {'config/accounts/' + selected + '/' + file for file in
                               ('dfs_credentials.json', 'dfs_refresh_pending.json')}
                    if implementation == 'official' and name == 'whoami':
                        allowed.add('config/accounts.json')
                    assert all(before.get(path) == after.get(path) for path in set(before) | set(after) if path not in allowed), 'cross-account or compute payload changed'
                    registry_after = json.loads((home / 'config/accounts.json').read_text())
                    expected_registry = copy.deepcopy(registry_before)
                    if implementation == 'official' and name == 'whoami':
                        meta = expected_registry['accounts'][selected]
                        meta.update(email=selected + '@observed.invalid', dfs_email=selected + '@observed.invalid', user_id='observed-' + selected)
                        updated = registry_after['accounts'][selected]['updated_at']
                        assert datetime.fromisoformat(updated).tzinfo is not None
                        meta['updated_at'] = updated
                    assert registry_after == expected_registry, (name, implementation, 'unexpected registry identity mutation')
                    observed = state(home, selected)
                    rotated = implementation == 'rust' and name != 'token-denied'
                    persisted = observed['refresh_pending'] if name == 'whoami-denied' and implementation == 'rust' else observed['credentials']
                    assert persisted['rotated_refresh_saved'] == persisted['fresh_access_saved'] == rotated
                    assert observed['credentials']['user_id'] == ('observed-' if name == 'whoami' else 'synthetic-') + selected
                    assert bool(observed['refresh_pending']) == (name == 'whoami-denied' and implementation == 'rust')
                    assert not server.errors, server.errors
                    assert len(server.trace) == (2 if whoami else 1)
                    assert server.trace[0]['scope_sent'] == (implementation == 'official')
                    result['state'], result['trace'] = observed, server.trace
                    result['registry_identity_updated'] = registry_after != registry_before
                    if implementation == 'rust' and name == 'whoami-denied':
                        recovered = subprocess.run([str(args.rust.resolve()), '--token-storage', 'file',
                                                    'drive', 'show', '--drive-account', selected],
                                                   env=env, cwd=home, capture_output=True, text=True, timeout=10)
                        assert recovered.returncode == 0, 'staged refresh could not recover through show'
                        summary = json.loads(recovered.stdout)
                        assert summary['user_id'] == 'synthetic-' + selected
                        assert summary['access_token'] == summary['refresh_token'] == '<set>'
                        assert not any(secret in recovered.stdout + recovered.stderr for secret in
                                       ('synthetic-access-', 'synthetic-rotated-', 'synthetic-secret-'))
                        recovery = state(home, selected)
                        assert recovery['refresh_pending'] is None
                        assert recovery['credentials']['rotated_refresh_saved'] and recovery['credentials']['fresh_access_saved']
                        assert len(server.trace) == 2, 'recovery unexpectedly replayed network requests'
                        final = snapshot(home)
                        assert all(before.get(path) == final.get(path) for path in set(before) | set(final) if path not in allowed)
                        result['recovery'] = {'state': recovery, 'additional_network_requests': 0}
                    row[implementation] = result
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=3)
                assert not thread.is_alive()
        rows.append(row)
    report = {'sources': sources, 'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(),
              'rust_native_keyring_compiled': False, 'cases': rows, 'official_runtime': runtime,
              'scope': 'Actual merged Drive refresh and userinfo against exact loopback; synthetic credentials only; no real authentication',
              'normalization': ['temporary HOME', 'loopback origin', 'JSON object order and whitespace', 'expiry checked against explicit bounded ranges'],
              'summary': {'cases': len(rows), 'contract_checks_passed': len(rows), 'unexplained_differences': 0},
              'intentional_differences': ['Rust omits scope on refresh', 'Rust persists rotated credentials, including recoverable staging before userinfo failure',
                                         'Rust does not substitute Drive identity into compute registry metadata', 'Rust does not inflate short expiry to 60 seconds',
                                         'Rust expiry is timezone-independent; upstream naive UTC timestamp is interpreted as local time',
                                         'Rust reports redacted errors instead of uncaught upstream exceptions']}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(report['summary']))


if __name__ == '__main__':
    main()
