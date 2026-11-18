#!/usr/bin/env python3
"""打包并运行本机原生发行候选；仅临时配置和 loopback，不发布或读取用户凭据。"""
import argparse
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import shlex
import subprocess
import sys
import tempfile
import threading
import time

from zipfile import ZipFile

from package_release import NOTICE_ARCHIVE, NOTICE_FILES, package, release_channel

ROOT = Path(__file__).resolve().parents[1]


def native_target():
    machine = {'arm64': 'aarch64', 'aarch64': 'aarch64', 'x86_64': 'x86_64', 'amd64': 'x86_64'}.get(platform.machine().lower())
    system = {'darwin': 'apple-darwin', 'linux': 'unknown-linux-gnu'}.get(sys.platform)
    if not machine or not system:
        raise ValueError('本机平台不在原生发行矩阵中')
    return machine + '-' + system


def executable_probe(binary, temporary, env):
    seen, errors = [], []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            from urllib.parse import parse_qs
            length = int(self.headers.get('Content-Length', '0'))
            value = parse_qs(self.rfile.read(min(length, 65537)).decode())
            seen.append(self.path)
            expected = {'grant_type': ['urn:ietf:params:oauth:grant-type:token-exchange'],
                        'audience': ['//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p'],
                        'requested_token_type': ['urn:ietf:params:oauth:token-type:access_token'],
                        'subject_token_type': ['urn:ietf:params:oauth:token-type:jwt'],
                        'subject_token': ['native-subject-fixture'],
                        'scope': ['openid https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/colaboratory https://www.googleapis.com/auth/drive.file']}
            if self.path != '/v1/token' or self.headers.get('Authorization') or value != expected:
                errors.append('native executable STS request mismatch')
            data = json.dumps({'access_token': 'native-access-fixture', 'expires_in': 3600,
                               'token_type': 'Bearer', 'issued_token_type': 'urn:ietf:params:oauth:token-type:access_token'}).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.02}, daemon=True)
    thread.start()
    try:
        marker = temporary / 'plugin-ran'
        payload = temporary / 'plugin-response.json'
        payload.write_text(json.dumps({'version': 1, 'success': True,
            'token_type': 'urn:ietf:params:oauth:token-type:id_token', 'id_token': 'native-subject-fixture',
            'expiration_time': int(time.time()) + 3600}))
        script = temporary / "plugin ' quoted.sh"
        script.write_text('printf ran > ' + shlex.quote(str(marker)) + '\n/bin/cat ' + shlex.quote(str(payload)) + '\n')
        adc = temporary / 'adc.json'
        adc.write_text(json.dumps({'type': 'external_account', 'audience': '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p',
            'subject_token_type': 'urn:ietf:params:oauth:token-type:jwt', 'token_url': f'http://127.0.0.1:{server.server_port}/v1/token',
            'credential_source': {'executable': {'command': '/bin/sh ' + shlex.quote(str(script))}}}))
        probe_env = dict(env, GOOGLE_APPLICATION_CREDENTIALS=str(adc))
        args = [str(binary), '--config-dir', str(temporary / 'config'), '--token-storage', 'file', '--auth', 'adc', 'account', 'login']
        denied = subprocess.run(args, env=dict(probe_env, GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES='0'), capture_output=True, text=True, timeout=15)
        assert denied.returncode == 1 and not seen and not marker.exists()
        allowed = subprocess.run(args, env=dict(probe_env, GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES='1'), capture_output=True, text=True, timeout=15)
        assert allowed.returncode == 0 and marker.exists() and seen == ['/v1/token'] and not errors
        assert not any(value in allowed.stdout + allowed.stderr for value in ('native-subject-fixture', 'native-access-fixture'))
        script.write_text("set -e\ncurrent=$(/bin/date +%s)\nexpiration=$((current+1))\n/bin/sleep 2\nprintf "
            + shlex.quote('{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:id_token","id_token":"native-subject-fixture","expiration_time":%s}')
            + ' "$expiration"\n')
        expired = subprocess.run(args, env=dict(probe_env, GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES='1'), capture_output=True, text=True, timeout=15)
        assert expired.returncode == 1 and '过期' in expired.stderr and seen == ['/v1/token'] and not errors, {'exit_code': expired.returncode, 'requests': len(seen), 'expiry_rejected': '过期' in expired.stderr, 'backend_errors': errors}
        return {'disabled_exit_code': denied.returncode, 'enabled_exit_code': allowed.returncode, 'expired_exit_code': expired.returncode, 'sts_requests': len(seen), 'expiry_rejected': True}

    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def aws_probe(binary, temporary, env):
    """Native binary AWS signing with environment snapshots; SDK-independent verifier."""
    from urllib.parse import parse_qs, unquote
    seen, errors = [], []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            seen.append(self.path)
            try:
                assert self.path == '/v1/token' and not self.headers.get('Authorization')
                length = int(self.headers.get('Content-Length', '0'))
                form = parse_qs(self.rfile.read(min(length, 65537)).decode())
                assert form['subject_token_type'] == ['urn:ietf:params:aws:token-type:aws4_request']
                value = json.loads(unquote(form['subject_token'][0]))
                headers = {item['key']: item['value'] for item in value['headers']}
                authorization = headers.pop('Authorization')
                assert value['url'] == 'https://sts.us-east-1.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15'
                assert value['method'] == 'POST'
                assert headers['x-goog-cloud-target-resource'] == form['audience'][0]
                assert headers['x-amz-security-token'] == 'native-aws-session'
                timestamp = headers['x-amz-date']
                signed = ';'.join(sorted(headers))
                canonical = '\n'.join(['POST', '/', 'Action=GetCallerIdentity&Version=2011-06-15', ''.join(f'{k}:{headers[k]}\n' for k in sorted(headers)), signed, hashlib.sha256(b'').hexdigest()])
                scope = timestamp[:8] + '/us-east-1/sts/aws4_request'
                message = '\n'.join(['AWS4-HMAC-SHA256', timestamp, scope, hashlib.sha256(canonical.encode()).hexdigest()])
                key = b'AWS4native-aws-secret'
                for component in (timestamp[:8], 'us-east-1', 'sts', 'aws4_request'):
                    key = hmac.new(key, component.encode(), hashlib.sha256).digest()
                assert authorization == f'AWS4-HMAC-SHA256 Credential=AKIDNATIVE/{scope}, SignedHeaders={signed}, Signature=' + hmac.new(key, message.encode(), hashlib.sha256).hexdigest()
            except (AssertionError, KeyError, ValueError, TypeError):
                errors.append('native AWS signed request mismatch')
            body = json.dumps({'access_token': 'native-aws-access', 'token_type': 'Bearer', 'expires_in': 3600, 'issued_token_type': 'urn:ietf:params:oauth:token-type:access_token'}).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.02}, daemon=True)
    thread.start()
    try:
        adc = temporary / 'aws-adc.json'
        adc.write_text(json.dumps({'type': 'external_account', 'audience': '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p',
            'subject_token_type': 'urn:ietf:params:aws:token-type:aws4_request', 'token_url': f'http://127.0.0.1:{server.server_port}/v1/token',
            'credential_source': {'environment_id': 'aws1', 'regional_cred_verification_url': 'https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15'}}))
        probe_env = dict(env, GOOGLE_APPLICATION_CREDENTIALS=str(adc), AWS_REGION='us-east-1', AWS_ACCESS_KEY_ID='AKIDNATIVE', AWS_SECRET_ACCESS_KEY='native-aws-secret', AWS_SESSION_TOKEN='native-aws-session')
        result = subprocess.run([str(binary), '--config-dir', str(temporary / 'aws-config'), '--token-storage', 'file', '--auth', 'adc', 'account', 'login'], env=probe_env, capture_output=True, text=True, timeout=15)
        assert result.returncode == 0 and seen == ['/v1/token'] and not errors
        assert not any(secret in result.stdout + result.stderr for secret in ('native-aws-secret', 'native-aws-session', 'native-aws-access'))
        return {'exit_code': result.returncode, 'sts_requests': len(seen), 'signature_verified': True}
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def validate(binary, debug_binary, base_url):
    target = native_target()
    with tempfile.TemporaryDirectory(prefix='colab-native-validation-') as directory:
        temporary = Path(directory)
        output = temporary / 'package'
        manifest = package(ROOT / 'Cargo.toml', [f'{target}={binary}'], base_url, output, allow_partial=True)
        artifact = manifest['artifacts'][target]
        packaged = output / Path(artifact['url']).name
        original = binary.read_bytes()
        assert packaged.read_bytes() == original
        assert artifact['size'] == len(original) and artifact['sha256'] == hashlib.sha256(original).hexdigest()
        checksummed = set()
        for row in (output / 'SHA256SUMS').read_text().splitlines():
            checksum, name = row.split('  ')
            assert hashlib.sha256((output / name).read_bytes()).hexdigest() == checksum
            checksummed.add(name)
        assert checksummed == {packaged.name, 'colab-update.json', NOTICE_ARCHIVE}
        assert {path.name for path in output.iterdir()} == checksummed | {'SHA256SUMS'}
        with ZipFile(output / NOTICE_ARCHIVE) as archive:
            assert archive.namelist() == list(NOTICE_FILES)
            for name in NOTICE_FILES:
                assert archive.read(name) == (ROOT / name).read_bytes()
        home = temporary / 'home'
        home.mkdir()
        env = {'PATH': os.defpath, 'HOME': str(home), 'SHELL': '/bin/zsh'}
        def run(args):
            return subprocess.run([str(packaged), *args], env=env, capture_output=True, text=True, check=True, timeout=15).stdout
        version = run(['version']).strip()
        assert version == 'Version: ' + manifest['version']
        for group in ('account', 'acct'):
            for alias in ('ls', 'rm', 'switch'):
                assert 'Usage:' in run([group, alias, '--help'])
        for command in ('add', 'login'):
            text = run(['account', command, '--help'])
            assert '--with-drive' in text and '--no-drive' in text
        for shell in ('bash', 'zsh', 'fish', 'powershell', 'pwsh'):
            assert 'colab' in run(['--show-completion', shell])
        inventory_args = ['--config-dir', str(temporary / 'inventory-config'), '--token-storage', 'file']
        inventory = json.loads(run([*inventory_args, 'account', 'list', '--json']))
        assert inventory['active'] == 'default' and len(inventory['accounts']) == 1
        assert inventory['accounts'][0]['auth']['ready'] is False
        assert inventory['accounts'][0]['sessions'] == []
        current = json.loads(run([*inventory_args, 'account', 'current', '--json']))
        assert current['source'] == 'active' and current['drive_source'] == 'self'
        assert Path(current['sessions_path']).name == 'sessions.json'
        unknown = json.loads(run(['--config-dir', str(temporary / 'inventory-config'), 'account', 'current', '--json']))
        assert unknown['auth']['ready'] is None and unknown['auth']['storage'] == 'keyring (not inspected)'
        registry = temporary / 'inventory-config/accounts.json'
        before = registry.read_bytes()
        rejected = subprocess.run([str(packaged), *inventory_args, '-a', 'missing', 'account', 'set-drive', 'default', '--default', '-a', 'default'],
            env=env, capture_output=True, text=True, timeout=15)
        assert rejected.returncode == 1 and not rejected.stdout and rejected.stderr.startswith("[colab] Account 'missing' not found")
        assert registry.read_bytes() == before
        for command in ('sessions', 'usage'):
            failure = json.loads(run([*inventory_args, command, '--json']))
            assert failure['account'] == 'default' and failure['error']
            assert failure['usage'] is None and failure['assignments'] == []
        # Exercise the packaged binary's Drive projection without compute auth,
        # using only private synthetic files and the explicit file backend.
        drive_path = registry.parent / 'accounts/default/dfs_credentials.json'
        drive_path.write_text(json.dumps({'flavor': 'custom', 'user_id': 'native-drive-user',
            'client_id': 'native-drive-client', 'client_secret': 'native-private-client-secret',
            'refresh_token': 'native-private-refresh', 'token': 'native-private-access',
            'token_uri': 'https://oauth2.googleapis.com/token',
            'scopes': ['https://www.googleapis.com/auth/drive'],
            'unknown_secret': 'native-private-unknown'}))
        drive_path.chmod(0o600)
        drive_output = run([*inventory_args, 'dfs', 'show'])
        drive = json.loads(drive_output)
        assert drive['account'] == drive['drive_account'] == 'default'
        assert drive['user_id'] == 'native-drive-user' and 'credentials' not in drive
        assert drive['access_token'] == drive['refresh_token'] == drive['client_secret'] == '<set>'
        assert 'native-private-' not in drive_output
        assert run([*inventory_args, 'drive', 'clear']).startswith('removed [default] ')
        assert not drive_path.exists()
        assert run([*inventory_args, 'drive', 'show']).strip().endswith(': empty (run `consent`)')
        assert run([*inventory_args, 'dfs', 'clear']).strip().endswith(': nothing to remove')
        assert registry.read_bytes() == before
        consent_args = [*inventory_args, '-a', 'native-new-drive', 'drive', 'consent',
                        '--url-only', '--no-browser', '--flavor', 'custom', '--client-id', 'native-consent-client',
                        '--auth-uri', 'http://127.0.0.1:9/auth', '--token-uri', 'http://127.0.0.1:9/token']
        from urllib.parse import parse_qs, urlsplit
        consent = parse_qs(urlsplit(run(consent_args).strip()).query)
        registered = json.loads(registry.read_text())
        assert registered['active'] == 'default' and list(registered['accounts']) == ['default', 'native-new-drive']
        pending_path = registry.parent / 'accounts/native-new-drive/dfs_pending.json'
        pending = json.loads(pending_path.read_text())
        assert pending['account'] == 'native-new-drive' and pending['state'] == consent['state'][0]
        assert consent['code_challenge_method'] == ['S256']
        assert not (pending_path.parent / 'oauth_token.json').exists()
        registered['accounts']['native-new-drive']['email'] = 'storage@example.invalid'
        registry.write_text(json.dumps(registered))
        before_retry = registry.read_bytes()
        retry = parse_qs(urlsplit(run(consent_args).strip()).query)
        assert retry['login_hint'] == ['storage@example.invalid']
        assert registry.read_bytes() == before_retry
        executable_adc = executable_probe(packaged, temporary, env)
        aws_adc = aws_probe(packaged, temporary, env)
        return {'version': manifest['version'], 'target': target,
            'release_binary_size': len(original), 'release_binary_sha256': artifact['sha256'],
            'debug_binary_size': debug_binary.stat().st_size, 'packaged_version_output': version,
            'executable_adc': executable_adc, 'aws_adc': aws_adc,
            'checks': ['packaged Drive metadata/clear and consent registration/hint',
                'packaged merged JSON account failures',
                'packaged root/local account validation preserves registry',
                'packaged account list/current projections', 'packaged offline native readiness remains unknown',
                'native release build', 'host native architecture', 'byte-identical packaged executable',
                'manifest size and SHA-256', 'SHA256SUMS for all candidate files',
                'byte-identical upstream and Rust dependency notices', 'packaged executable version',
                'packaged account aliases help', 'packaged joint account flow options help', 'packaged shell completion scripts',
                'packaged executable ADC opt-in and loopback STS exchange',
                'packaged executable ADC expiry during execution', 'packaged AWS ADC SigV4 and loopback STS'],
            'source': 'local cargo build --release --locked; tools/validate_native_release.py',
            'download_base_url': base_url, 'published': False,
            'limitations': ['single host validation only', 'GitHub workflow not run', 'no remote release download',
                            'no platform signing or notarization verification', 'no real IdP or Google authentication']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/colab')
    parser.add_argument('--debug-binary', type=Path, default=ROOT / 'target/debug/colab')
    parser.add_argument('--base-url', help='覆盖 Cargo 仓库与版本推导出的下载目录')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/native-release-validation.json')
    args = parser.parse_args()
    base_url = args.base_url or release_channel(ROOT / 'Cargo.toml')['download_base_url']
    result = validate(args.binary.resolve(), args.debug_binary.resolve(), base_url)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({'target': result['target'], 'release_size': result['release_binary_size'], 'checks': len(result['checks'])}))


if __name__ == '__main__':
    main()
