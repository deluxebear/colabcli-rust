#!/usr/bin/env python3
"""在临时配置和 loopback 服务上，对比 Rust CLI 与固定官方 CLI。不会使用真实凭据或 VM。"""
import argparse
import ast
import base64
import collections
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import struct
import sys
import subprocess
import tempfile
import threading
import time
from urllib.parse import parse_qs, quote, unquote, urlsplit
import uuid

ROOT = Path(__file__).resolve().parents[1]
COMMON = ['new', 'sessions', 'status', 'stop', 'ls', 'upload', 'download', 'rm', 'edit', 'exec', 'repl', 'auth', 'drivemount', 'install', 'ssh', 'restart-kernel', 'run', 'log', 'usage', 'url', 'pay', 'readme', 'skill', 'version', 'update', 'help']
EXTENSIONS = ['kernels', 'runs', 'run-cleanup', 'automation-cleanup', 'drive', 'ext']
DRIVES = ['consent', 'import', 'show', 'token', 'clear', 'status', 'probe-cleanup', 'mount', 'umount', 'leases', 'lease-cleanup']
EXT_COMMANDS = ['doctor', 'install', 'version']
ACCOUNTS = ['status', 'list', 'current', 'add', 'use', 'set-drive', 'rename', 'remove', 'login', 'repair']
FIXTURE = b'compare fixture\n\x00\xff'
SSH_KEY = 'ssh-ed25519 fixture-key compare-fixture'
SSH_BYTES = b'fixture-ssh\x00\xff\r\n'
SSH_INPUT = 'client-ssh-你好'


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding='utf-8')
    path.chmod(0o600)


class Backend(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, scenario):
        super().__init__(('127.0.0.1', 0), Handler)
        self.base = 'http://127.0.0.1:' + str(self.server_port)
        self.scenario = scenario
        self.trace = []
        self.errors = []
        self.active = scenario.get('active', True)
        self.files = {'content/data.bin': FIXTURE, 'content/edit.txt': b'before\n'}
        self.accelerator = 'NONE'
        self.variant = 0
        self.shape = 0
        self.executed = []
        self.counter = 0

    def handle_error(self, _request, _client_address):
        error = sys.exc_info()[1]
        if not isinstance(error, (BrokenPipeError, ConnectionResetError)):
            self.errors.append('loopback request failed: ' + type(error).__name__)

    def assignment(self, endpoint='ep-compare'):
        return {'endpoint': endpoint, 'accelerator': self.accelerator, 'variant': self.variant, 'machineShape': self.shape,
                'runtimeProxyInfo': {'token': 'compare-proxy-fixture', 'tokenExpiresInSeconds': 3600, 'url': self.base + '/runtime'}}

    def kernel(self):
        return {'id': 'kernel-compare', 'name': 'python3', 'last_activity': '2026-11-16T00:00:00Z', 'execution_state': 'idle', 'connections': 1}


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_args):
        pass

    def send_json(self, value, status=200):
        body = b'' if value is None else json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        if self.server.scenario.get('metadata') and urlsplit(self.path).path in ('/', '/computeMetadata/v1/instance/service-accounts/default/token') and not self.server.scenario.get('metadata_spoof'):
            self.send_header('Metadata-Flavor', 'Google')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_request(self):
        s = self.server
        parsed = urlsplit(self.path)
        path, query = unquote(parsed.path), {k: v[-1] for k, v in parse_qs(parsed.query).items()}
        length = int(self.headers.get('Content-Length', '0'))
        raw = self.rfile.read(length) if length else b''
        body = (json.loads(raw) if 'application/json' in self.headers.get('Content-Type', '') else {k: v[-1] for k, v in parse_qs(raw.decode()).items()}) if raw else None
        # Persist only synthetic protocol fields, never raw credential headers.
        s.trace.append({'method': self.command, 'path': path, 'query': {k: v for k, v in query.items() if 'token' not in k},
                        'content_length': self.headers.get('Content-Length'), 'body': ({'grant_type': body.get('grant_type')} if path.endswith('/token') and body else body),
                        'oauth': self.headers.get('Authorization') == 'Bearer compare-oauth-fixture',
                        'xsrf': self.headers.get('X-Goog-Colab-Token') == 'compare-xsrf-fixture',
                        'proxy': query.get('colab-runtime-proxy-token') == 'compare-proxy-fixture' or self.headers.get('X-Colab-Runtime-Proxy-Token') == 'compare-proxy-fixture'})
        if path == '/colab/ssh':
            agents = parse_qs(parsed.query).get('colab-client-agent', [])
            s.trace[-1]['query']['colab-client-agent'] = agents
            if agents != ['colab-cli', 'ssh-mode-' + s.scenario.get('ssh_mode', 'proxy')]:
                s.errors.append('SSH agent query mismatch')
            if self.headers.get('X-Colab-Ssh-Pubkey') != SSH_KEY or self.headers.get('X-Colab-Client-Agent') != 'colab-cli':
                s.errors.append('SSH public key/client agent header mismatch')
            if self.headers.get('Authorization') or self.headers.get('X-Colab-Runtime-Proxy-Token'):
                s.errors.append('unexpected authorization header on SSH transport')
            if s.scenario.get('ssh_status'):
                return self.send_json({'error': 'already-active-session', 'limit': 3}, s.scenario['ssh_status'])
        if self.headers.get('Upgrade', '').lower() == 'websocket':
            return self.websocket()
        if path == '/drive/token':
            if s.scenario.get('drive_refresh_fail'):
                return self.send_json({'error': 'invalid_grant'}, 400)
            return self.send_json({'access_token': 'synthetic-drive-access', 'refresh_token': 'synthetic-drive-rotated', 'expires_in': 3600, 'token_type': 'Bearer', 'scope': 'openid https://www.googleapis.com/auth/drive'})
        if path == '/drive/userinfo':
            if self.headers.get('Authorization') != 'Bearer synthetic-drive-access':
                s.errors.append('wrong token identity sent to Drive userinfo')
            return self.send_json({'sub': 'gaia-drive-fixture', 'email': 'drive@example.invalid'})
        if s.scenario.get('metadata') and path in ('/', '/computeMetadata/v1/instance/service-accounts/default/token'):
            if self.command != 'GET' or self.headers.get('Authorization') or self.headers.get('Metadata-Flavor') != 'Google':
                s.errors.append('metadata request had wrong method/flavor or account authorization')
            if path == '/':
                return self.send_json({})
            expected_scopes = 'openid,https://www.googleapis.com/auth/userinfo.profile,https://www.googleapis.com/auth/userinfo.email,https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/colaboratory,https://www.googleapis.com/auth/drive.file'
            if query != {'scopes': expected_scopes}:
                s.errors.append('metadata request scope query mismatch')
            return self.send_json({'access_token': 'compare-oauth-fixture', 'token_type': 'Bearer', 'expires_in': 3600})
        if path == '/external/subject':
            if self.headers.get('Authorization') != 'Bearer external-subject-header':
                s.errors.append('external subject header missing')
            return self.send_json({'jwt': 'external-subject-fixture'})
        if path == '/v1/oauth/token':
            expected_auth = 'Basic ' + base64.b64encode(b'workforce-client:workforce-secret').decode()
            if self.headers.get('Authorization') != expected_auth or body != {'grant_type': 'refresh_token', 'refresh_token': 'workforce-refresh-fixture'}:
                s.errors.append('workforce STS Basic authentication or refresh body mismatch')
            if s.scenario.get('workforce_status'):
                return self.send_json({'private': 'private-workforce-error'}, s.scenario['workforce_status'])
            return self.send_json({'access_token': 'compare-oauth-fixture', 'expires_in': 3600, 'refresh_token': 'workforce-rotated-fixture', 'token_type': 'Bearer'})
        if s.scenario.get('external_aws') and path.startswith('/latest/'):
            if self.headers.get('Authorization'):
                s.errors.append('AWS metadata received Google bearer')
            if path == '/latest/api/token':
                if self.command != 'PUT' or self.headers.get('X-aws-ec2-metadata-token-ttl-seconds') != '300' or self.headers.get('X-aws-ec2-metadata-token'):
                    s.errors.append('AWS IMDSv2 PUT mismatch')
                if s.scenario.get('aws_v2_failure'):
                    return self.send_json({'private': 'private-aws-error'}, 403)
                raw = b'imds-aws-fixture'
                self.send_response(200)
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
                return
            if self.headers.get('X-aws-ec2-metadata-token') != 'imds-aws-fixture':
                s.errors.append('AWS metadata missing v2 session header')
            if path == '/latest/meta-data/iam/security-credentials':
                raw = b'role-fixture'
                self.send_response(200)
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
                return
            if path == '/latest/meta-data/iam/security-credentials/role-fixture':
                return self.send_json({'AccessKeyId': 'AKIDFIXTURE', 'SecretAccessKey': 'synthetic-aws-secret', 'Token': 'synthetic-aws-session'})
            s.errors.append('unexpected AWS metadata path')
            return self.send_json({}, 404)
        if path == '/v1/token' and s.scenario.get('external_aws'):
            if self.headers.get('Authorization') or self.headers.get('X-aws-ec2-metadata-token') or body.get('subject_token_type') != 'urn:ietf:params:aws:token-type:aws4_request':
                s.errors.append('AWS STS request header/type mismatch')
            try:
                signed_request = json.loads(unquote(body['subject_token']))
                headers = {item['key']: item['value'] for item in signed_request['headers']}
                authorization = headers.pop('Authorization')
                audience = '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p'
                if signed_request['url'] != 'https://sts.us-east-1.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15' or signed_request['method'] != 'POST' or headers['x-goog-cloud-target-resource'] != audience or headers['x-amz-security-token'] != 'synthetic-aws-session':
                    raise ValueError('AWS signed request fields')
                timestamp = headers['x-amz-date']
                signed = ';'.join(sorted(headers))
                canonical = '\n'.join(['POST', '/', 'Action=GetCallerIdentity&Version=2011-06-15', ''.join(f'{k}:{headers[k]}\n' for k in sorted(headers)), signed, hashlib.sha256(b'').hexdigest()])
                scope = timestamp[:8] + '/us-east-1/sts/aws4_request'
                message = '\n'.join(['AWS4-HMAC-SHA256', timestamp, scope, hashlib.sha256(canonical.encode()).hexdigest()])
                key = b'AWS4synthetic-aws-secret'
                for component in (timestamp[:8], 'us-east-1', 'sts', 'aws4_request'):
                    key = hmac.new(key, component.encode(), hashlib.sha256).digest()
                expected = f'AWS4-HMAC-SHA256 Credential=AKIDFIXTURE/{scope}, SignedHeaders={signed}, Signature=' + hmac.new(key, message.encode(), hashlib.sha256).hexdigest()
                if authorization != expected:
                    raise ValueError('AWS signature')
                expected_scope = 'https://www.googleapis.com/auth/cloud-platform' if s.scenario.get('external_iam') else 'openid https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/colaboratory https://www.googleapis.com/auth/drive.file'
                if body.get('scope') != expected_scope or body.get('audience') != audience or body.get('grant_type') != 'urn:ietf:params:oauth:grant-type:token-exchange' or body.get('requested_token_type') != 'urn:ietf:params:oauth:token-type:access_token':
                    raise ValueError('AWS exchange fields')
            except (KeyError, ValueError, TypeError):
                s.errors.append('AWS signed subject or STS body invalid')
            return self.send_json({'access_token': 'compare-oauth-fixture', 'expires_in': 3600, 'token_type': 'Bearer', 'issued_token_type': 'urn:ietf:params:oauth:token-type:access_token'})
        if path == '/v1/token':
            scopes = 'https://www.googleapis.com/auth/cloud-platform' if s.scenario.get('external_iam') else 'openid https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/colaboratory https://www.googleapis.com/auth/drive.file'
            expected = {'grant_type': 'urn:ietf:params:oauth:grant-type:token-exchange', 'audience': '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p', 'requested_token_type': 'urn:ietf:params:oauth:token-type:access_token', 'subject_token_type': 'urn:ietf:params:oauth:token-type:jwt', 'subject_token': 'external-subject-fixture', 'scope': scopes}
            if body != expected or self.headers.get('Authorization'):
                s.errors.append('external STS form or authorization mismatch')
            return self.send_json({'access_token': 'compare-oauth-fixture', 'token_type': 'Bearer', 'issued_token_type': 'urn:ietf:params:oauth:token-type:access_token', 'expires_in': 3600})
        if s.scenario.get('adc_kind') == 'impersonated_service_account' or s.scenario.get('external_iam'):
            s.trace[-1]['target'] = self.headers.get('Authorization') == 'Bearer impersonated-target-fixture'
            if path.startswith('/tun/m/') and not s.trace[-1]['target']:
                s.errors.append('source or unrelated identity reached the Colab API')
        if path.startswith('/v1/projects/-/serviceAccounts/') and path.endswith(':generateAccessToken'):
            if self.command != 'POST' or self.headers.get('Authorization') != 'Bearer compare-oauth-fixture':
                s.errors.append('IAM exchange did not use the source bearer')
            expected_scopes = ['openid', 'https://www.googleapis.com/auth/userinfo.profile', 'https://www.googleapis.com/auth/userinfo.email', 'https://www.googleapis.com/auth/cloud-platform', 'https://www.googleapis.com/auth/colaboratory', 'https://www.googleapis.com/auth/drive.file']
            if body != {'scope': expected_scopes, 'lifetime': '3600s'}:
                s.errors.append('IAM scope/lifetime or direct delegation body mismatch')
            if s.scenario.get('iam_status'):
                return self.send_json({'error': 'private-iam-error'}, s.scenario['iam_status'])
            return self.send_json({'accessToken': 'impersonated-target-fixture', 'expireTime': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(time.time() + 3600))})
        if path == '/tokeninfo':
            expected_token = 'impersonated-target-fixture' if (s.scenario.get('adc_kind') == 'impersonated_service_account' or s.scenario.get('external_iam')) else 'compare-oauth-fixture'
            if self.headers.get('Authorization') or query.get('access_token') != expected_token:
                s.errors.append('tokeninfo must use query token without bearer header')
            if s.scenario.get('tokeninfo_status'):
                return self.send_json({'error': 'invalid_token', 'private': 'synthetic-private-tokeninfo-error'}, s.scenario['tokeninfo_status'])
            return self.send_json({'email': 'compute@example.invalid', 'aud': 'compare-client', 'expires_in': '3599', 'scope': 'z-scope a-scope', 'private': 'synthetic-private-tokeninfo-result'})
        if path == '/native-update.json':
            if s.scenario.get('native_update_http'):
                return self.send_json({'private': 'private-update-error'}, s.scenario['native_update_http'])
            if s.scenario.get('native_update_bad'):
                return self.send_json({'private': 'private-update-error'})
            artifact = {'url': s.base + '/native-colab', 'size': 64, 'sha256': hashlib.sha256(b'x' * 64).hexdigest()}
            targets = ['aarch64-apple-darwin', 'x86_64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu']
            return self.send_json({'schema_version': 1, 'package': 'colabcli-rust', 'version': s.scenario.get('native_update_version', '0.2.0'), 'artifacts': {target: artifact for target in targets}})
        if path == '/oauth/token':
            return self.send_json({'access_token': 'compare-oauth-fixture', 'refresh_token': 'compare-refresh-fixture', 'expires_in': 3600, 'token_type': 'Bearer', 'scope': 'openid'})
        if path == '/tun/m/assign':
            if s.scenario.get('allocation_status'):
                return self.send_json({}, s.scenario['allocation_status'])
            if self.command == 'GET':
                return self.send_json({'acc': '', 'nbh': 'canonical', 'variant': 'DEFAULT', 'token': 'compare-xsrf-fixture'})
            if self.headers.get('Content-Length') != '0':
                return self.send_json({}, 411)
            s.active = True
            s.accelerator = query['accelerator']
            s.variant = {'DEFAULT': 0, 'GPU': 1, 'TPU': 2}[query['variant']]
            s.shape = int(query.get('shape') == 'hm')
            return self.send_json(s.assignment())
        if path == '/tun/m/assignments':
            if s.scenario.get('list_status'):
                return self.send_json({}, s.scenario['list_status'])
            assignments = [s.assignment()] if s.active else []
            if s.scenario.get('second'):
                assignments.append(s.assignment('ep-second'))
            if s.scenario.get('orphan'):
                assignments.append(s.assignment('ep-orphan'))
            return self.send_json({'assignments': assignments})
        if path.startswith('/tun/m/unassign/'):
            if self.command == 'GET':
                return self.send_json({'token': 'compare-xsrf-fixture'})
            s.active = False
            return self.send_json(None, 204)
        if path == '/tun/m/ccu-info':
            return self.send_json({'currentBalance': 12.5, 'consumptionRateHourly': 1.25, 'assignmentsCount': int(s.active)})
        if path.startswith('/runtime/api/contents/'):
            name = path.removeprefix('/runtime/api/contents/')
            if self.command == 'GET':
                if name == 'content':
                    entries = [{'name': 'z.bin', 'path': 'content/z.bin', 'type': 'file'}, {'name': 'folder', 'path': 'content/folder', 'type': 'directory'}, {'name': 'a.txt', 'path': 'content/a.txt', 'type': 'file'}]
                    return self.send_json({'name': 'content', 'path': 'content', 'type': 'directory', 'content': entries})
                if name not in s.files:
                    return self.send_json({}, 404)
                return self.send_json({'name': name.rsplit('/', 1)[-1], 'path': name, 'type': 'file', 'format': 'base64', 'content': base64.encodebytes(s.files[name]).decode()})
            if self.command == 'PUT':
                s.files[name] = base64.b64decode(body['content'])
                return self.send_json(body)
            if name not in s.files:
                return self.send_json({}, 404)
            del s.files[name]
            return self.send_json(None, 204)
        if path == '/runtime/api/kernels':
            if self.command == 'POST' and s.scenario.get('drive_status'):
                return self.send_json({**s.kernel(), 'id': 'drive-probe-fixture'}, 201)
            return self.send_json([s.kernel()] if self.command == 'GET' else s.kernel(), 200 if self.command == 'GET' else 201)
        if path.startswith('/runtime/api/kernels/'):
            return self.send_json(None, 204) if self.command == 'DELETE' else self.send_json(s.kernel())
        if path == '/runtime/api':
            return self.send_json({'version': '2.14.2'})
        if path == '/runtime/api/kernelspecs':
            return self.send_json({'default': 'python3', 'kernelspecs': {'python3': {'name': 'python3', 'spec': {'argv': [], 'display_name': 'Python 3', 'language': 'python'}, 'resources': {}}}})
        if path == '/runtime/api/sessions':
            return self.send_json([] if self.command == 'GET' else {'id': 'session-compare', 'path': 'content', 'name': '', 'type': 'notebook', 'kernel': s.kernel()})
        s.errors.append('unexpected ' + self.command + ' ' + path)
        self.send_json({}, 404)

    def do_GET(self):
        self.handle_request()

    do_POST = do_PUT = do_DELETE = do_PATCH = do_GET

    def read_exact(self, length):
        data = self.rfile.read(length)
        if len(data) != length:
            raise EOFError
        return data

    def frame(self, opcode, payload):
        size = len(payload)
        length = bytes([size]) if size < 126 else (b'\x7e' + struct.pack('!H', size) if size < 65536 else b'\x7f' + struct.pack('!Q', size))
        self.wfile.write(bytes([0x80 | opcode]) + length + payload)
        self.wfile.flush()

    def websocket(self):
        key = self.headers['Sec-WebSocket-Key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
        self.send_response(101)
        self.send_header('Upgrade', 'websocket')
        self.send_header('Connection', 'Upgrade')
        self.send_header('Sec-WebSocket-Accept', base64.b64encode(hashlib.sha1(key.encode()).digest()).decode())
        self.end_headers()
        self.connection.settimeout(10)
        self.close_connection = True
        is_ssh = urlsplit(self.path).path == '/colab/ssh'
        ssh_input = b''
        try:
            while True:
                first, second = self.read_exact(2)
                opcode, size = first & 15, second & 127
                if size == 126:
                    size = struct.unpack('!H', self.read_exact(2))[0]
                elif size == 127:
                    size = struct.unpack('!Q', self.read_exact(8))[0]
                if size > 4 * 1024 * 1024:
                    raise ValueError('mock websocket frame too large')
                mask = self.read_exact(4) if second & 128 else b''
                payload = self.read_exact(size)
                if mask:
                    payload = bytes(v ^ mask[i % 4] for i, v in enumerate(payload))
                if opcode == 8:
                    self.frame(8, payload)
                    return
                if opcode == 9:
                    self.frame(10, payload)
                    continue
                if is_ssh:
                    if opcode == 10:
                        self.frame(8, b'')
                        return
                    if opcode == 2:
                        ssh_input += payload
                        if len(ssh_input) >= len(SSH_INPUT.encode()):
                            if ssh_input != SSH_INPUT.encode():
                                self.server.errors.append('SSH stdin bytes changed')
                            self.frame(2, SSH_BYTES)
                            self.frame(9, b'control-not-output')
                    continue
                if opcode == 2:
                    count = struct.unpack('!I', payload[:4])[0]
                    offsets = struct.unpack('!' + 'I' * count, payload[4:4 + count * 4])
                    payload = payload[offsets[0]:offsets[1] if count > 1 else len(payload)]
                if opcode in (1, 2):
                    self.reply_message(json.loads(payload))
        except (EOFError, BrokenPipeError, ConnectionResetError, TimeoutError):
            pass
        except Exception as exc:
            self.server.errors.append('websocket: ' + repr(exc))

    def reply_message(self, request):
        server = self.server
        header = request['header']
        kind = header['msg_type']

        def send(kind, content, channel='iopub'):
            msg = {'header': {'msg_id': str(uuid.uuid4()), 'msg_type': kind, 'session': header['session'], 'username': 'kernel', 'date': '2026-11-16T00:00:00Z', 'version': '5.3'},
                   'parent_header': header, 'metadata': {}, 'content': content, 'channel': channel, 'msg_type': kind, 'buffers': []}
            msg['msg_id'] = msg['header']['msg_id']
            self.frame(1, json.dumps(msg).encode())

        send('status', {'execution_state': 'busy'})
        if kind == 'kernel_info_request':
            send('kernel_info_reply', {'status': 'ok', 'protocol_version': '5.3', 'implementation': 'mock', 'implementation_version': '1', 'language_info': {'name': 'python', 'version': '3.12', 'mimetype': 'text/x-python', 'file_extension': '.py'}, 'banner': '', 'help_links': []}, 'shell')
        elif kind == 'execute_request':
            code = request['content']['code']
            server.executed.append(code)
            server.counter += 1
            is_error = 'RAISE_FIXTURE' in code
            error_name, error_detail, error_trace = 'ValueError', 'fixture error', ['ValueError: fixture error']
            if server.scenario.get('install_syntax'):
                try:
                    ast.parse(code)
                except SyntaxError:
                    is_error = True
                    error_name, error_detail = 'SyntaxError', 'synthetic invalid Python source'
                    error_trace = ['SyntaxError: synthetic invalid Python source']
            if server.scenario.get('install_args') is not None and 'def install():' in code and not is_error:
                send('stream', {'name': 'stdout', 'text': 'Installation Complete (via uv)!\n'})
            if 'SILENCE_FIXTURE' in code:
                return
            if 'DELAYED_OUTPUT_FIXTURE' in code:
                time.sleep(0.7)
                send('stream', {'name': 'stdout', 'text': 'delayed output\n'})
            if 'REPEATED_OUTPUT_FIXTURE' in code:
                for index in range(7):
                    time.sleep(0.1)
                    send('stream', {'name': 'stdout', 'text': f'progress {index}\n'})
            if 'IMAGE_FIXTURE' in code:
                send('display_data', {'data': {'image/png': 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRz8AAAAASUVORK5CYII=\n'}, 'metadata': {}})
            # Fixed synthetic outputs. Never eval arbitrary submitted code.
            if 'fixture-output' in code:
                send('stream', {'name': 'stdout', 'text': 'fixture-output\n'})
            marker = re.search(r'application/vnd\.colab-cli\.run-result\+json', code)
            if marker:
                literals = [node.args[0].value for node in ast.walk(ast.parse(code)) if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == 'loads' and node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)]
                nonce = next(json.loads(literal)['nonce'] for literal in literals if 'nonce' in json.loads(literal))
                send('display_data', {'data': {'application/vnd.colab-cli.run-result+json': {'nonce': nonce, 'exit_code': int(is_error)}}, 'metadata': {}})
            if server.scenario.get('drive_status') and '_colab_cli_drivefs_operation' in code:
                literals = [node.args[0].value for node in ast.walk(ast.parse(code)) if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == 'loads' and node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)]
                payload = next(json.loads(value) for value in literals if 'request' in json.loads(value))
                if payload['action'] != 'status':
                    server.errors.append('Drive probe submitted a mutation')
                send('display_data', {'data': {'application/vnd.colab-cli.drivefs-result+json': {
                    'version': 1, 'operation_id': payload['request']['operation_id'], 'mountpoint': payload['request']['mountpoint'],
                    'ok': True, 'stage': 'unmanaged', 'error': None, 'mounted': False, 'managed': False,
                    'unknown_secret': 'synthetic-private-drive-status'}}, 'metadata': {}})
            if is_error:
                send('error', {'ename': error_name, 'evalue': error_detail, 'traceback': error_trace})
            content = {'status': 'error' if is_error else 'ok', 'execution_count': server.counter, 'user_expressions': {}, 'payload': []}
            if is_error:
                content.update(ename=error_name, evalue=error_detail, traceback=error_trace)
            send('execute_reply', content, 'shell')
        else:
            server.errors.append('unexpected websocket message ' + kind)
        send('status', {'execution_state': 'idle'})


def clean_env(home):
    return {'PATH': os.defpath, 'HOME': str(home), 'LANG': 'en_US.UTF-8', 'LC_ALL': 'en_US.UTF-8',
            'TERM': 'dumb', 'NO_COLOR': '1', 'COLUMNS': '160', 'PYTHONIOENCODING': 'utf-8',
            'TMPDIR': str(home), 'NO_PROXY': '*'}


def invoke(command, env, cwd, stdin='', timeout=25, ssh_proxy=False):
    started = time.monotonic()
    try:
        if ssh_proxy:
            process = subprocess.Popen(list(map(str, command)), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=cwd)
            writer = process.stdin
            try:
                writer.write(stdin.encode())
                writer.flush()
                process.stdin = None  # keep our writer open until the remote closes
                stdout, stderr = process.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate()
                raise
            finally:
                writer.close()
            return {'exit_code': process.returncode, 'stdout': stdout.decode('utf-8', 'backslashreplace'), 'stderr': stderr.decode('utf-8', 'backslashreplace'),
                    'stdout_base64': base64.b64encode(stdout).decode(), 'duration_seconds': round(time.monotonic() - started, 3)}
        process = subprocess.run(list(map(str, command)), input=stdin, text=True, capture_output=True, env=env, cwd=cwd, timeout=timeout)
        return {'exit_code': process.returncode, 'stdout': process.stdout, 'stderr': process.stderr,
                'duration_seconds': round(time.monotonic() - started, 3)}
    except subprocess.TimeoutExpired as exc:
        return {'exit_code': None, 'stdout': (exc.stdout or b'').decode() if isinstance(exc.stdout, bytes) else exc.stdout or '',
                'stderr': 'comparison subprocess timed out', 'duration_seconds': round(time.monotonic() - started, 3)}


def normalize(text, temp):
    text = re.sub(r'\x1b\[[0-9;]*m', '', text)
    text = text.replace(str(temp), '<TEMP>')
    text = re.sub(r'http://127\.0\.0\.1:\d+', '<MOCK>', text)
    text = re.sub(r'\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b', '<UUID>', text)
    return text


def setup(root, implementation, backend, scenario):
    root.mkdir(parents=True)
    profile = root / 'config/accounts/default' if implementation == 'rust' else root / 'config'
    profile.mkdir(parents=True)
    if implementation == 'rust':
        write_json(root / 'config/accounts.json', {'version': 1, 'active': 'default', 'default_drive_account': None, 'accounts': {'default': {'auth_provider': 'oauth2'}}})
        write_json(profile / 'oauth_token.json', {'token': 'compare-oauth-fixture', 'refresh_token': 'compare-refresh-fixture', 'client_id': 'compare-client', 'client_secret': 'compare-secret-fixture', 'expiry': '2099-01-01T00:00:00Z', 'token_uri': backend.base + '/oauth/token', 'scopes': ['openid']})
    if scenario.get('merged_inventory') and implementation == 'rust':
        registry = json.loads((root / 'config/accounts.json').read_text())
        registry['accounts']['secondary'] = {'auth_provider': 'oauth2'}
        write_json(root / 'config/accounts.json', registry)
        write_json(root / 'config/accounts/secondary/oauth_token.json', json.loads((profile / 'oauth_token.json').read_text()))
        write_json(root / 'config/accounts/secondary/sessions.json', {})
    if scenario.get('drive_status_storage') and implementation == 'rust':
        registry = json.loads((root / 'config/accounts.json').read_text())
        registry['accounts']['secondary'] = {'auth_provider': 'adc', 'adc_credentials_file': str(root / 'must-not-read.json')}
        write_json(root / 'config/accounts.json', registry)
        target = root / 'config/accounts/secondary/dfs_credentials.json'
        target.parent.mkdir(parents=True)
        target.write_text('synthetic-private-drive-status-credentials-invalid-json')
    if scenario.get('metadata') and implementation == 'rust':
        registry = json.loads((root / 'config/accounts.json').read_text())
        registry['accounts']['default']['auth_provider'] = 'adc'
        if scenario.get('metadata_explicit'):
            registry['accounts']['default']['adc_credentials_file'] = str(root / 'explicit-missing-adc.json')
        write_json(root / 'config/accounts.json', registry)
    if scenario.get('adc_kind') and implementation == 'rust':
        adc = {'type': scenario['adc_kind'], 'client_email': 'service@example.invalid', 'private_key': 'synthetic-invalid-private-key', 'credential_source': {'executable': {'command': 'must-not-run'}}}
        if scenario['adc_kind'] == 'impersonated_service_account':
            adc.update(service_account_impersonation_url=backend.base + '/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken', source_credentials={'type': 'authorized_user', 'client_id': 'compare-client', 'client_secret': 'compare-secret-fixture', 'refresh_token': 'compare-refresh-fixture', 'token_uri': backend.base + '/oauth/token'})
        if scenario.get('external_valid'):
            source = {'file': 'external-subject.txt'}
            (root / 'external-subject.txt').write_text('external-subject-fixture')
            if scenario.get('external_url'):
                source = {'url': backend.base + '/external/subject', 'headers': {'Authorization': 'Bearer external-subject-header'}, 'format': {'type': 'json', 'subject_token_field_name': 'jwt'}}
            if scenario.get('external_executable'):
                response_file = root / 'executable-response.json'
                write_json(response_file, {'version': 1, 'success': True, 'token_type': 'urn:ietf:params:oauth:token-type:id_token', 'id_token': 'external-subject-fixture', 'expiration_time': int(time.time()) + 3600})
                script = root / 'credential plugin.sh'
                script.write_text('printf ran > ' + shlex.quote(str(root / 'executable-ran')) + '\ncat ' + shlex.quote(str(response_file)) + '\n')
                source = {'executable': {'command': '/bin/sh ' + shlex.quote(str(script))}}
            adc = {'type': 'external_account', 'audience': '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p', 'subject_token_type': 'urn:ietf:params:oauth:token-type:jwt', 'token_url': backend.base + '/v1/token', 'credential_source': source}
            if scenario.get('external_iam'):
                adc['service_account_impersonation_url'] = backend.base + '/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken'
        if scenario.get('external_aws'):
            adc['subject_token_type'] = 'urn:ietf:params:aws:token-type:aws4_request'
            adc['credential_source'] = {'environment_id': 'aws1', 'regional_cred_verification_url': 'https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15', 'url': backend.base + '/latest/meta-data/iam/security-credentials', 'imdsv2_session_token_url': backend.base + '/latest/api/token'}
        if scenario['adc_kind'] == 'external_account_authorized_user':
            adc = {'type': 'external_account_authorized_user', 'audience': '//iam.googleapis.com/locations/global/workforcePools/p/providers/p', 'token_url': backend.base + '/v1/oauth/token', 'client_id': 'workforce-client', 'client_secret': 'workforce-secret', 'refresh_token': 'workforce-refresh-fixture'}
        write_json(root / 'adc.json', adc)
        registry = json.loads((root / 'config/accounts.json').read_text())
        registry['accounts']['default'].update(auth_provider='adc', adc_credentials_file=str(root / 'adc.json'))
        write_json(root / 'config/accounts.json', registry)
    if scenario.get('check', '').startswith('native-update-'):
        installation = root / 'bin' / 'native-colab'
        installation.parent.mkdir(exist_ok=True)
        installation.write_bytes(b'synthetic-native-update-target')
        installation.chmod(0o700)
    sessions = {}
    if scenario.get('local', True):
        sessions['compare'] = {'name': 'compare', 'endpoint': 'ep-compare', 'token': 'compare-proxy-fixture', 'url': backend.base + '/runtime',
                               'token_expires_at': '2099-01-01T00:00:00Z', 'accelerator': 'NONE', 'variant': 'DEFAULT', 'machine_shape': 'STANDARD',
                               'kernel_id': 'kernel-compare', 'session_id': 'session-compare', 'last_execution': None, 'running': None}
    if scenario.get('second'):
        sessions['second'] = {**sessions['compare'], 'name': 'second', 'endpoint': 'ep-second'}
    if scenario.get('automation_pending'):
        sessions['compare']['automation_pending'] = {'operation': 'auth', 'source_hash': '0' * 64, 'kernel_id': 'automation-owned', 'session_id': 'automation-session', 'phase': 'unknown', 'msg_id': 'automation-message', 'server-private': 'must-not-display'}
        sessions['compare']['running'] = 'automation-message'
    if scenario.get('drive_probe_phase'):
        sessions['compare']['drive_probe'] = {'version': 1, 'endpoint': 'ep-compare', 'session_id': 'probe-session',
            'kernel_id': None if scenario['drive_probe_phase'] == 'creating' else 'drive-probe-fixture',
            'phase': scenario['drive_probe_phase'], 'unknown_secret': 'synthetic-private-drive-probe'}
    if scenario.get('fresh_kernel'):
        sessions['compare']['kernel_id'] = None
        sessions['compare']['session_id'] = None
    if scenario.get('run_intent'):
        owner = '11111111-1111-4111-8111-111111111111'
        sessions['compare']['run_owner'] = owner
        write_json(profile / f'runs/{owner}.json', {'id': owner, 'account': 'default', 'session': 'compare', 'endpoint': 'ep-compare', 'keep': False, 'created_at': '2026-11-16T00:00:00Z'})
    if scenario.get('drive_credentials'):
        write_json(profile / 'dfs_credentials.json', {'flavor': 'imported', 'refresh_token': 'synthetic-drive-refresh', 'client_id': 'drive-fixture-client', 'client_secret': 'synthetic-drive-client-secret', 'token_uri': backend.base + '/drive/token', 'scopes': ['openid', 'https://www.googleapis.com/auth/drive'], 'user_id': 'gaia-drive-fixture', 'email': 'drive@example.invalid', 'private_unknown_field': 'must-not-display'})
    write_json(profile / 'sessions.json', sessions)
    history = profile / 'history'
    history.mkdir()
    if scenario.get('history'):
        event = {'timestamp': '2026-11-16T00:00:00+00:00', 'event_type': 'execution', 'code': "print('fixture-output')", 'outputs': [{'output_type': 'stream', 'name': 'stdout', 'text': 'fixture-output\n'}]}
        (history / 'compare.jsonl').write_text(json.dumps(event) + '\n')
    (root / '.ssh').mkdir()
    (root / '.ssh/id_ed25519.pub').write_text(SSH_KEY + '\n')
    (root / 'identity.pub').write_text(SSH_KEY + '\n')
    (root / 'bin').mkdir(exist_ok=True)
    ssh = root / 'bin/ssh'
    ssh.write_text("#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$FIXTURE_SSH_ARGUMENTS\"\nexit 23\n")
    ssh.chmod(0o700)
    (root / 'requirements.txt').write_bytes(b'demo==1.2\n')
    (root / 'requirements #中.txt').write_bytes(b'demo==1.2\n')
    (root / 'input.bin').write_bytes(FIXTURE)
    (root / 'script.py').write_text("print('fixture-output')\n" + ('RAISE_FIXTURE\n' if scenario.get('script_error') else ''))
    write_json(root / 'notebook.ipynb', {'nbformat': 4, 'nbformat_minor': 5, 'metadata': {}, 'cells': [{'id': 'fixture-cell', 'cell_type': 'code', 'metadata': {}, 'source': "print('fixture-output')", 'outputs': [], 'execution_count': None}]})
    editor = root / 'editor'
    editor.write_text('#!/bin/sh\nprintf "after\\n" > "$1"\n')
    editor.chmod(0o700)
    write_json(root / 'client.json', {'installed': {'client_id': 'compare-client', 'client_secret': 'compare-secret-fixture',
               'auth_uri': backend.base + '/oauth/authorize', 'token_uri': backend.base + '/oauth/token', 'redirect_uris': ['http://localhost']}})
    if scenario.get('browser'):
        for launcher in ['open', 'xdg-open']:
            script = root / 'bin' / launcher
            script.write_text('#!/bin/sh\nprintf \'%s\\n\' "$1" >> "$FIXTURE_BROWSER_URLS"\nprintf \'ignored-browser-output\'\n')
            script.chmod(0o700)
    if scenario.get('account_email') and implementation == 'rust':
        registry = json.loads((root / 'config/accounts.json').read_text())
        registry['accounts']['default']['email'] = 'compute+test@example.invalid'
        write_json(root / 'config/accounts.json', registry)
    return profile


def wire(trace):
    result = []
    for item in trace:
        entry = {key: item[key] for key in ('method', 'path', 'query', 'oauth', 'xsrf', 'proxy')}
        entry['query'] = dict(entry['query'])
        if 'target' in item:
            entry['target'] = item['target']
        for volatile in ['nbh', 'session_id', 'session']:
            if volatile in entry['query']:
                entry['query'][volatile] = '<IDENTITY>'
        if entry['method'] == 'POST' and entry['path'].startswith('/tun/m/'):
            entry['content_length'] = item['content_length']
        if '/api/contents/' in entry['path'] and entry['method'] == 'PUT':
            entry['body'] = item['body']
        result.append(entry)
    return result


def cases():
    result = []

    def add(name, command, **kwargs):
        result.append({'name': name, 'command': command, **kwargs})

    for command in COMMON + EXTENSIONS + ['account']:
        add('help-' + command, [command, '--help'], help=True, extension=command in EXTENSIONS or command == 'account')
    for command in ACCOUNTS:
        add('help-account-' + command, ['account', command, '--help'], help=True, extension=True)
    for command in DRIVES:
        add('help-drive-' + command, ['drive', command, '--help'], help=True, extension=True)
    for command in EXT_COMMANDS:
        add('help-ext-' + command, ['ext', command, '--help'], help=True, extension=True)
        add('help-drive-legacy-' + command, ['drive', command, '--help'], help=True, extension=True)
    add('help-ext-status', ['ext', 'status', '--help'], help=True, extension=True)
    add('ext-version', ['ext', 'version'], extension=True, check='ext-version')
    add('drive-legacy-version', ['drive', 'version'], extension=True, check='ext-version')
    add('ext-install', ['ext', 'install', '--site-dir', 'python-must-not-create', '--json'], extension=True, check='ext-install')
    add('ext-install-quiet', ['ext', 'install', '--quiet'], extension=True, check='ext-quiet')
    add('drive-legacy-install', ['drive', 'install', '--json'], extension=True, check='ext-install')
    add('ext-doctor', ['ext', 'doctor', '--json', '--strict'], extension=True, drive_credentials=True, check='ext-doctor')
    add('ext-status-alias', ['ext', 'status', '--json'], extension=True, drive_credentials=True, check='ext-doctor')
    add('dfs-legacy-doctor', ['dfs', 'doctor', '--json'], extension=True, drive_credentials=True, check='ext-doctor')
    add('ext-doctor-pending', ['ext', 'doctor', '--json'], extension=True, automation_pending=True, check='ext-doctor')
    add('ext-doctor-strict-missing', ['ext', 'doctor', '--json', '--strict'], extension=True, expect=1, check='ext-doctor')
    add('ext-doctor-keyring', ['ext', 'doctor', '--json'], extension=True, token_storage='keyring', check='ext-doctor')
    add('help-whoami', ['whoami', '--help'], help=True)
    add('url-named', ['url', '-s', 'compare'], check='utility-url')
    add('url-unique', ['url'], check='utility-url')
    add('url-pruned-unique', ['url'], second=True, active=False, url_endpoint='ep-second', check='utility-url')
    add('url-host', ['url', '-s', 'compare', '--host', 'https://sandbox.example.invalid/'], check='utility-url')
    add('url-open', ['url', '-s', 'compare', '--open'], browser=True, check='utility-url')
    add('url-missing', ['url', '-s', 'missing'], expect=1)
    add('url-ambiguous', ['url'], second=True, expect=1)
    add('url-account-email', ['url', '-s', 'compare'], account_email=True, extension=True, check='utility-url')
    add('url-invalid-origin', ['url', '-s', 'compare', '--host', 'https://sandbox.invalid/path'], extension=True, expect=1, check='drive-probe-no-wire')
    add('pay', ['pay'], browser=True, check='utility-pay')
    add('pay-url-only', ['pay', '--url-only'], extension=True, check='utility-pay')
    add('readme', ['readme'], check='utility-resource')
    add('readme-alias', ['README'], check='utility-resource')
    add('skill', ['skill'], check='utility-resource')
    add('skill-alias', ['SKILL'], check='utility-resource')
    add('whoami', ['whoami'], check='utility-whoami')
    add('whoami-json', ['whoami', '--json'], extension=True, check='utility-whoami')
    add('whoami-rejected', ['whoami'], tokeninfo_status=400, expect=1, check='utility-whoami-error')
    add('help-root', ['--help'], help=True)
    add('version', ['version'])
    add('drive-show-empty', ['drive', 'show'], extension=True, check='drive-empty')
    add('drive-show', ['drive', 'show'], extension=True, drive_credentials=True, check='drive-summary')
    add('drive-alias', ['dfs', 'show'], extension=True, drive_credentials=True, check='drive-summary')
    add('drive-clear', ['drive', 'clear'], extension=True, drive_credentials=True, check='drive-cleared')
    add('drive-clear-empty', ['drive', 'clear'], extension=True, check='drive-cleared')
    add('drive-token', ['drive', 'token', '--json'], extension=True, drive_credentials=True, check='drive-token')
    add('drive-token-whoami', ['drive', 'token', '--whoami'], extension=True, drive_credentials=True, check='drive-token')
    add('drive-token-failed', ['drive', 'token'], extension=True, drive_credentials=True, drive_refresh_fail=True, expect=1)
    add('drive-token-missing', ['drive', 'token'], extension=True, expect=1)
    add('drive-import-invalid', ['drive', 'import'], extension=True, expect=1)
    add('drive-consent-url', ['drive', 'consent', '--url-only'], extension=True, check='drive-pending')
    add('drive-consent-empty-code', ['drive', 'consent', '--code', 'synthetic-code'], extension=True, expect=1)
    add('drive-leases-empty', ['drive', 'leases'], extension=True, check='drive-leases-empty')
    add('drive-mount-dry-run', ['drive', 'mount', '-s', 'compare', '--dry-run'], extension=True, drive_credentials=True, check='drive-mount-dry-run')
    add('drive-mount-env-preview', ['drive', 'mount', '-s', 'compare', '--from-env', '--dry-run'], extension=True, drive_env=True, check='drive-mount-dry-run')
    add('drive-mount-env-implicit-preview', ['drive', 'mount', '-s', 'compare', '--dry-run'], extension=True, drive_env=True, drive_credentials=True, check='drive-mount-dry-run')
    add('drive-mount-env-incomplete', ['drive', 'mount', '-s', 'compare', '--from-env', '--dry-run'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-mount-cli-token-scope', ['drive', 'mount', '-s', 'compare', '--cli-token', '--user-id', 'fixture-gaia', '--dry-run'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-mount-source-conflict', ['drive', 'mount', '-s', 'compare', '--cli-token', '--from-env', '--dry-run'], extension=True, expect=2, check='drive-probe-no-wire')
    add('drive-mount-native-extra-preview', ['drive', 'mount', '-s', 'compare', '--via-cli', '--drive-extra=--curl_logging', '--user-id', 'override-gaia', '--email', 'override@example.invalid', '--dry-run'], extension=True, drive_credentials=True, check='drive-mount-dry-run')
    add('drive-mount-extra-protected', ['drive', 'mount', '-s', 'compare', '--drive-extra=--preferences=mount_point_path:/other'], extension=True, drive_credentials=True, expect=1, check='drive-probe-no-wire')
    add('drive-mount-missing-credentials', ['drive', 'mount', '-s', 'compare'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-mount-invalid-path', ['drive', 'mount', '-s', 'compare', '--path', '/content/../drive'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-umount-invalid-path', ['drive', 'umount', '-s', 'compare', '--path', '/content'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-lease-cleanup-invalid-endpoint', ['drive', 'lease-cleanup', '--endpoint', ''], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-status', ['drive', 'status', '-s', 'compare', '--json'], extension=True, drive_status=True, check='drive-status')
    for label, command, extra in [
        ('override', ['drive', 'status', '-s', 'compare', '--drive-account', 'secondary'], {'drive_status_storage': True}),
        ('alias', ['dfs', 'status', '-s', 'compare', '--drive-account', 'secondary'], {'drive_status_storage': True}),
        ('env', ['drive', 'status', '-s', 'compare'], {'drive_status_storage': True, 'drive_status_env': True}),
        ('precedence', ['drive', 'status', '-s', 'compare', '--drive-account', 'default'], {'drive_status_storage': True, 'drive_status_env': True}),
    ]:
        add('drive-status-account-' + label, command, extension=True, drive_status=True, check='drive-status', **extra)
    add('drive-status-account-invalid', ['drive', 'status', '-s', 'compare', '--drive-account', 'missing'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-status-during-mutation', ['drive', 'status', '-s', 'compare'], extension=True, drive_status=True, automation_pending=True, check='drive-status')
    add('drive-status-invalid-path', ['drive', 'status', '-s', 'compare', '--path', '/content/../drive'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drive-probe-cleanup-empty', ['drive', 'probe-cleanup', '-s', 'compare'], extension=True, check='drive-probe-empty')
    add('drive-probe-cleanup-owned', ['drive', 'probe-cleanup', '-s', 'compare'], extension=True, drive_probe_phase='ready', automation_pending=True, check='drive-probe-cleaned')
    add('drive-probe-cleanup-unknown', ['drive', 'probe-cleanup', '-s', 'compare'], extension=True, drive_probe_phase='creating', expect=1, check='drive-probe-no-wire')
    add('drive-probe-cleanup-conflict', ['drive', 'probe-cleanup', '-s', 'compare', '--kernel-id', 'kernel-compare'], extension=True, drive_probe_phase='ready', expect=1, check='drive-probe-no-wire')
    add('drive-status-probe-create-unknown', ['drive', 'status', '-s', 'compare'], extension=True, drive_probe_phase='creating', expect=1, check='drive-probe-no-wire')
    add('drive-probe-session-status', ['status', '-s', 'compare', '--json'], extension=True, drive_probe_phase='creating', check='drive-probe-session-status')
    add('automation-cleanup-empty', ['automation-cleanup', '-s', 'compare', '--json'], extension=True)
    add('automation-cleanup-owned', ['automation-cleanup', '-s', 'compare', '--json'], extension=True, automation_pending=True, check='automation-cleaned')
    add('automation-cleanup-conflict', ['automation-cleanup', '-s', 'compare', '--kernel-id', 'unrelated'], extension=True, automation_pending=True, check='automation-refused', expect=1)
    add('automation-status-json', ['status', '-s', 'compare', '--json'], extension=True, automation_pending=True, check='automation-status')
    add('automation-status-text', ['status', '-s', 'compare'], extension=True, automation_pending=True, check='automation-status-text')

    add('install-packages', ['install', '-s', 'compare', 'demo==1.2', 'extra>=1'], check='install-executed', install_args=['demo==1.2', 'extra>=1'])
    add('install-requirements', ['install', '-s', 'compare', '-r', 'requirements.txt', 'extra>=1'], check='install-executed', install_args=['-r', '/content/requirements.txt', 'extra>=1'], install_file='content/requirements.txt')
    add('install-unicode-requirements', ['install', '-s', 'compare', '-r', 'requirements #中.txt'], check='install-executed', install_args=['-r', '/content/requirements #中.txt'], install_file='content/requirements #中.txt')
    add('install-empty', ['install', '-s', 'compare'], expect=1)
    add('install-missing-requirement', ['install', '-s', 'compare', '-r', 'missing.txt'], expect=1)
    add('install-json', ['install', '-s', 'compare', 'demo', '--json'], extension=True, check='install-executed', install_args=['demo'])
    add('install-quoted-arguments', ['install', '-s', 'compare', "quote'package"], check='install-executed', install_args=["quote'package"], install_syntax=True)
    add('auth-authorized', ['auth', '-s', 'compare'], fresh_kernel=True)
    add('auth-existing-kernel', ['auth', '-s', 'compare'])
    add('drivemount-authorized', ['drivemount', '-s', 'compare'], fresh_kernel=True, native_upstream=True)
    add('drivemount-existing-kernel', ['drivemount', '-s', 'compare'], native_upstream=True)
    add('drivemount-default-missing', ['drivemount', '-s', 'compare'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drivemount-no-fallback', ['drivemount', '-s', 'compare', '--no-fallback'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drivemount-preview', ['drivemount', '-s', 'compare', '--dry-run'], extension=True, drive_credentials=True, check='drive-mount-dry-run')
    add('drivemount-upstream-conflict', ['drivemount', '-s', 'compare', '--upstream', '--readonly'], extension=True, expect=1, check='drive-probe-no-wire')
    add('drivemount-upstream-stored', ['drivemount', '-s', 'compare', '--upstream'], extension=True, drive_credentials=True, check='drivemount-upstream')
    add('drivemount-env-official', ['drivemount', '-s', 'compare'], extension=True, drive_credentials=True, drive_default='off', check='drivemount-upstream')
    add('drivemount-legacy-official', ['drivemount', '-s', 'compare'], extension=True, drive_default='false', legacy_drive_default=True, check='drivemount-upstream')
    add('drivemount-env-conflict', ['drivemount', '-s', 'compare', '--dry-run'], extension=True, drive_default='off', expect=1, check='drive-probe-no-wire')
    add('help-command', ['help', 'new'])
    for name, opts in [('cpu', []), ('gpu', ['--gpu', 't4']), ('tpu', ['--tpu', 'v5e1']), ('high-ram', ['--high-mem']), ('l4-high-ram', ['--gpu', 'L4', '--high-mem'])]:
        add('new-' + name, ['new', '-s', 'compare', *opts], local=False, active=False, check='allocated')
    add('new-default-name', ['new'], local=False, active=False, check='allocated-default')
    add('new-412', ['new', '-s', 'compare'], local=False, active=False, allocation_status=412, expect=1)
    add('new-gpu-400', ['new', '-s', 'compare', '--gpu', 'A100'], local=False, active=False, allocation_status=400, expect=1)
    add('new-tpu-400', ['new', '-s', 'compare', '--tpu', 'v5e1'], local=False, active=False, allocation_status=400, expect=1)
    add('new-invalid-hardware', ['new', '--gpu', 'invalid'], local=False, active=False, expect='nonzero')
    add('new-gpu-tpu-conflict', ['new', '--gpu', 'T4', '--tpu', 'v5e1'], local=False, active=False, expect=0, check='allocated-default')
    add('new-bare-gpu-option', ['new', '-s', 'compare', '--gpu'], local=False, active=False, expect=2)
    for command in ['sessions', 'status']:
        add(command + '-empty', [command], local=False, active=False)
        add(command + '-named', [command])
        add(command + '-orphan', [command], local=False, orphan=True, active=False)
        add(command + '-stale', [command], active=False, check='pruned')
        add(command + '-http-error', [command], list_status=503, expect='nonzero')
        add(command + '-json', [command, '--json'], extension=True)
        add(command + '-all', [command, '--all', '--json'], extension=True)
    add('status-missing', ['status', '-s', 'missing'])
    add('stop-named', ['stop', '-s', 'compare'], check='released')
    add('stop-missing', ['stop', '-s', 'missing'])
    add('stop-without-selector', ['stop'])
    add('stop-empty', ['stop'], local=False, active=False, expect=1)
    add('stop-multiple', ['stop'], second=True, expect=1)
    add('usage', ['usage'])
    add('usage-json', ['usage', '--json'], extension=True)
    add('usage-all', ['usage', '--all', '--json'], extension=True)
    add('ls-unique', ['ls', 'content'])
    add('ls-directory', ['ls', '-s', 'compare', 'content'])
    add('ls-file', ['ls', '-s', 'compare', 'content/data.bin'])
    add('ls-missing', ['ls', '-s', 'compare', 'content/missing'], expect=1)
    add('ls-json', ['ls', '-s', 'compare', '--json'], extension=True)
    add('upload-binary', ['upload', '-s', 'compare', 'input.bin', 'content/upload.bin'], check='uploaded')
    add('upload-unicode-path', ['upload', '-s', 'compare', 'input.bin', 'content/中 #?%.bin'], check='uploaded-unicode')
    add('upload-missing', ['upload', '-s', 'compare', 'missing.bin', 'content/upload.bin'], expect=1)
    add('download-binary', ['download', '-s', 'compare', 'content/data.bin', 'download.bin'], check='downloaded')
    add('download-missing', ['download', '-s', 'compare', 'content/missing', 'download.bin'], expect=1)
    add('rm-file', ['rm', '-s', 'compare', 'content/data.bin'], check='deleted')
    add('rm-missing', ['rm', '-s', 'compare', 'content/missing'], expect=1)
    add('edit-file', ['edit', '-s', 'compare', 'content/edit.txt'], check='edited')
    add('ssh-stale-endpoint', ['ssh', '--proxy-mode', '-s', 'compare', '--colab-expected-endpoint', 'old-ep'], extension=True, expect=1, check='ssh-no-connection')
    add('ssh-child-missing', ['ssh', '--proxy-mode', '--colab-internal-mode', 'interactive', '--colab-expected-endpoint', 'original-ep', '-s', 'missing'], extension=True, expect=2, check='ssh-no-connection')
    add('ssh-proxy', ['ssh', '--proxy-mode', '-s', 'compare', '-i', 'identity.pub'], stdin=SSH_INPUT, ssh_proxy=True, check='ssh-bytes')
    add('ssh-proxy-unique', ['ssh', '--proxy-mode', '-i', 'identity.pub'], stdin=SSH_INPUT, ssh_proxy=True, check='ssh-bytes')
    add('ssh-proxy-created', ['ssh', '--proxy-mode', '-s', 'compare', '-i', 'identity.pub'], local=False, active=False, stdin=SSH_INPUT, ssh_proxy=True, check='ssh-bytes')
    add('ssh-proxy-rm', ['ssh', '--proxy-mode', '-s', 'compare', '--rm', '-i', 'identity.pub'], stdin=SSH_INPUT, ssh_proxy=True, check='ssh-released')
    add('ssh-proxy-created-rm', ['ssh', '--proxy-mode', '-s', 'compare', '--rm', '-i', 'identity.pub'], local=False, active=False, stdin=SSH_INPUT, ssh_proxy=True, check='ssh-released')
    add('ssh-proxy-empty', ['ssh', '--proxy-mode'], local=False, active=False, expect=1)
    add('ssh-proxy-internal-cmd-missing', ['ssh', '--proxy-mode', '--colab-internal-mode', 'cmd', '-s', 'missing'], expect=2)
    add('ssh-proxy-command-conflict', ['ssh', '--proxy-mode', '--', 'printf', 'hi'], expect=2)
    for status in [400, 401, 403, 404, 429, 502]:
        add('ssh-upgrade-' + str(status), ['ssh', '--proxy-mode', '-s', 'compare', '-i', 'identity.pub'], stdin=SSH_INPUT, ssh_proxy=True, ssh_status=status, expect=1)
    add('ssh-upgrade-rm', ['ssh', '--proxy-mode', '-s', 'compare', '--rm', '-i', 'identity.pub'], stdin=SSH_INPUT, ssh_proxy=True, ssh_status=404, expect=1, check='ssh-cleanup-difference')
    add('ssh-new-key-failure-rm', ['ssh', '--proxy-mode', '-s', 'compare', '--rm', '-i', 'missing-key'], local=False, active=False, expect=2, check='ssh-cleanup-difference')
    add('ssh-command-argv', ['ssh', '-s', 'compare', '--rm', '--gpu', 'invalid-ignored', '--', 'printf', 'a b', '--flag'], expect=23, check='ssh-command', remote="cd /content 2>/dev/null; printf 'a b' --flag", pty='-T')
    add('ssh-command-shell', ['ssh', '-s', 'compare', '--', 'printf hello; true'], expect=23, check='ssh-command', remote='cd /content 2>/dev/null; printf hello; true', pty='-T')
    add('ssh-interactive-existing', ['ssh', '-s', 'compare', '--rm'], expect=23, check='ssh-command', remote='cd /content 2>/dev/null; exec ${SHELL:-/bin/bash} -l', pty='-t')
    add('ssh-interactive-created', ['ssh', '--rm'], local=False, active=False, expect=23, check='ssh-command-released', remote='cd /content 2>/dev/null; exec ${SHELL:-/bin/bash} -l', pty='-t')
    add('repl-stdin', ['repl', '-s', 'compare'], stdin="print('fixture-output')", check='executed')
    add('repl-unique', ['repl'], stdin="print('fixture-output')", check='executed')
    add('repl-empty', ['repl', '-s', 'compare'], stdin='')
    add('repl-error', ['repl', '-s', 'compare'], stdin='RAISE_FIXTURE')
    add('repl-image', ['repl', '-s', 'compare', '--output-image', 'output.png'], stdin='IMAGE_FIXTURE', check='image')
    add('repl-missing', ['repl', '-s', 'missing'], stdin='pass', expect=1)
    add('exec-unique', ['exec'], stdin="print('fixture-output')", check='executed')
    add('exec-missing', ['exec', '-s', 'missing'], stdin='pass', expect=1)
    add('exec-stdin', ['exec', '-s', 'compare'], stdin="print('fixture-output')", check='executed')
    add('exec-file', ['exec', '-s', 'compare', '-f', 'script.py'], check='executed')
    add('exec-notebook', ['exec', '-s', 'compare', '-f', 'notebook.ipynb'], check='notebook')
    add('exec-env', ['exec', '-s', 'compare', '--env', 'TEST=value'], stdin="print('fixture-output')", check='executed')
    add('exec-invalid-env', ['exec', '-s', 'compare', '--env', 'INVALID'], stdin='pass', expect='nonzero')
    add('exec-error', ['exec', '-s', 'compare'], stdin='RAISE_FIXTURE')
    add('exec-idle-timeout', ['exec', '-s', 'compare', '--timeout', '0.2'], stdin='SILENCE_FIXTURE', expect=1, check='exec-timeout-diagnostic')
    add('exec-idle-timeout-zero', ['exec', '-s', 'compare', '--timeout', '0'], stdin='DELAYED_OUTPUT_FIXTURE', expect_stdout='delayed output\n')
    add('exec-idle-timeout-progress', ['exec', '-s', 'compare', '--timeout', '0.4'], stdin='REPEATED_OUTPUT_FIXTURE', expect_stdout=''.join(f'progress {index}\n' for index in range(7)))
    add('exec-image', ['exec', '-s', 'compare', '--output-image', 'output.png'], stdin='IMAGE_FIXTURE', check='image')
    add('exec-error-json', ['exec', '-s', 'compare', '--json'], stdin='RAISE_FIXTURE', extension=True, expect=1)
    add('exec-empty', ['exec', '-s', 'compare'], stdin='')
    add('exec-json', ['exec', '-s', 'compare', '--json'], stdin="print('fixture-output')", extension=True, check='executed')
    add('kernels-list', ['kernels', '-s', 'compare', '--json'], extension=True)
    add('restart-kernel', ['restart-kernel', '-s', 'compare'])
    add('restart-fresh', ['restart-kernel', '-s', 'compare'], fresh_kernel=True)
    add('run-release', ['run', '-s', 'compare', 'script.py', '--fixture-arg'], local=False, active=False, check='run-released')
    add('run-default-name', ['run', 'script.py'], local=False, active=False, check='run-released')
    add('run-error', ['run', '-s', 'compare', 'script.py'], local=False, active=False, script_error=True, expect=1, check='run-released')
    add('run-keep', ['run', '-s', 'compare', '--keep', 'script.py'], local=False, active=False, check='run-kept')
    add('run-missing-script', ['run', 'missing.py'], expect='nonzero')
    add('run-no-script', ['run'], expect=2)
    add('runs-empty', ['runs'], extension=True)
    add('runs-pending', ['runs'], extension=True, run_intent=True)
    add('run-cleanup-owned', ['run-cleanup', '11111111-1111-4111-8111-111111111111'], extension=True, run_intent=True, check='run-cleaned')
    add('run-cleanup-invalid', ['run-cleanup', 'invalid'], extension=True, expect=1)
    add('log-empty', ['log'], local=False)
    add('log-list', ['log'], history=True)
    add('log-session', ['log', '-s', 'compare'], history=True)
    add('log-filter', ['log', '-s', 'compare', '-t', 'execution', '-n', '1'], history=True)
    add('log-missing', ['log', '-s', 'missing'])
    for suffix in ['jsonl', 'txt', 'md', 'ipynb']:
        add('log-export-' + suffix, ['log', '-s', 'compare', '-o', 'export.' + suffix], history=True, check='exported')
    for label, args in [('list', ['list', '--json']), ('current', ['current', '--json']), ('add', ['add', 'work', '--configure-only']), ('use', ['use', 'default']), ('set-drive', ['set-drive', 'default', '--default']), ('clear-drive', ['set-drive', '--default', '--clear']), ('repair', ['repair']), ('login-url', ['login', '--url-only']), ('rename-missing', ['rename', 'missing', 'new']), ('remove-missing', ['remove', 'missing'])]:
        add('account-' + label, ['account', *args], extension=True, expect=1 if label in ['rename-missing', 'remove-missing'] else 0)
    add('account-status', ['account', 'status'], extension=True, check='account-status')
    add('account-status-json', ['account', 'status', '--json'], extension=True, drive_credentials=True, check='account-status')
    add('account-status-orphan', ['account', 'status', '--json'], extension=True, orphan=True, check='account-status')
    add('account-status-empty', ['account', 'status', '--json'], extension=True, local=False, active=False, check='account-status')
    add('account-status-list-failure', ['account', 'status', '--json'], extension=True, list_status=502, check='account-status')
    add('alias-acct-status', ['acct', 'status', '--json'], extension=True, check='account-status')
    add('adc-service-invalid-key', ['account', 'login'], extension=True, adc_kind='service_account', expect=1, check='adc-no-wire')
    add('adc-service-doctor', ['ext', 'doctor', '--json'], extension=True, adc_kind='service_account', check='adc-doctor')
    add('adc-service-status-error', ['account', 'status', '--json'], extension=True, adc_kind='service_account', check='adc-status-error')
    add('adc-external-unsupported', ['sessions', '--json'], extension=True, adc_kind='external_account', expect=0, inventory_error=True, check='adc-no-wire')
    for label, command, extra in [
        ('login', ['account', 'login'], {}),
        ('sessions', ['sessions', '--json'], {}),
        ('whoami', ['whoami', '--json'], {}),
        ('status', ['account', 'status', '--json'], {}),
        ('iam-failure', ['sessions', '--json'], {'iam_status': 403, 'expect': 0, 'inventory_error': True}),
        ('doctor', ['ext', 'doctor', '--json'], {}),
    ]:
        add('adc-impersonation-' + label, command, extension=True, adc_kind='impersonated_service_account', check='adc-impersonation-' + label, **extra)
    for label, command, extra in [
        ('login', ['account', 'login'], {}),
        ('sessions', ['sessions', '--json'], {}),
        ('whoami', ['whoami', '--json'], {'external_url': True}),
        ('status', ['account', 'status', '--json'], {'external_iam': True}),
        ('iam-failure', ['sessions', '--json'], {'external_iam': True, 'iam_status': 403, 'expect': 0, 'inventory_error': True}),
        ('doctor', ['ext', 'doctor', '--json'], {}),
    ]:
        add('adc-external-' + label, command, extension=True, adc_kind='external_account', external_valid=True, check='adc-external-' + label, **extra)
    for label, command, extra in [
        ('login', ['account', 'login'], {}),
        ('sessions', ['sessions', '--json'], {}),
        ('whoami', ['whoami', '--json'], {}),
        ('status', ['account', 'status', '--json'], {}),
        ('refresh-failure', ['sessions', '--json'], {'workforce_status': 400, 'expect': 0, 'inventory_error': True}),
        ('doctor', ['ext', 'doctor', '--json'], {}),
    ]:
        add('adc-workforce-user-' + label, command, extension=True, adc_kind='external_account_authorized_user', check='adc-workforce-user-' + label, **extra)
    for label, command, extra in [
        ('login', ['account', 'login'], {'metadata_proxy': True}),
        ('sessions', ['sessions', '--json'], {}),
        ('whoami', ['whoami', '--json'], {}),
        ('status', ['account', 'status', '--json'], {}),
        ('disabled', ['sessions', '--json'], {'metadata_disabled': True, 'expect': 0, 'inventory_error': True}),
        ('explicit-missing', ['sessions', '--json'], {'metadata_explicit': True, 'expect': 0, 'inventory_error': True}),
        ('spoofed', ['sessions', '--json'], {'metadata_spoof': True, 'expect': 0, 'inventory_error': True}),
        ('doctor', ['ext', 'doctor', '--json'], {}),
    ]:
        add('adc-metadata-' + label, command, extension=True, metadata=True, check='adc-metadata-' + label, **extra)
    for label, command, extra in [
        ('login', ['account', 'login'], {}),
        ('sessions', ['sessions', '--json'], {}),
        ('whoami', ['whoami', '--json'], {}),
        ('status', ['account', 'status', '--json'], {'external_iam': True}),
        ('disabled', ['sessions', '--json'], {'executable_disabled': True, 'expect': 0, 'inventory_error': True}),
        ('doctor', ['ext', 'doctor', '--json'], {}),
    ]:
        add('adc-executable-' + label, command, extension=True, adc_kind='external_account', external_valid=True, external_executable=True, check='adc-executable-' + label, **extra)
    for label, command, extra in [
        ('login', ['account', 'login'], {}),
        ('sessions', ['sessions', '--json'], {}),
        ('whoami', ['whoami', '--json'], {}),
        ('status', ['account', 'status', '--json'], {'external_iam': True}),
        ('imds', ['sessions', '--json'], {'aws_metadata': True}),
        ('v2-failure', ['sessions', '--json'], {'aws_metadata': True, 'aws_v2_failure': True, 'expect': 0, 'inventory_error': True}),
        ('doctor', ['ext', 'doctor', '--json'], {}),
    ]:
        add('adc-aws-' + label, command, extension=True, adc_kind='external_account', external_valid=True, external_aws=True, check='adc-aws-' + label, **extra)
    for label, command, extra in [
        ('check', ['update', '--json'], {'native_update_source': True}),
        ('env-check', ['update', '--json'], {'native_update_env': True}),
        ('up-to-date', ['update', '--install', '--json', '--install-path', 'bin/native-colab'], {'native_update_source': True, 'native_update_version': '0.1.0'}),
        ('status', ['update', '--status', '--install-path', 'bin/native-colab'], {'native_update_bad_env': True}),
        ('no-source', ['update', '--json'], {'expect': 1, 'token_storage': 'keyring'}),
        ('bad-manifest', ['update', '--json'], {'native_update_source': True, 'native_update_bad': True, 'expect': 1}),
        ('http-failure', ['update', '--json'], {'native_update_source': True, 'native_update_http': 503, 'expect': 1}),
        ('install-conflict', ['update', '--install', '--rollback'], {'expect': 2}),
        ('install-native-required', ['update', '--install', '--json', '--install-path', 'bin/native-colab'], {'native_update_source': True, 'expect': 1}),
        ('timeout-invalid', ['update', '--timeout', '0'], {'expect': 2}),
    ]:
        add('native-update-' + label, command, extension=True, check='native-update-' + label, **extra)
    for group in ('account', 'acct'):
        add('merged-alias-' + group + '-ls', [group, 'ls', '--json'], extension=True)
        add('merged-alias-' + group + '-switch', [group, 'switch', 'default'], extension=True)
        add('merged-alias-' + group + '-rm', [group, 'rm', 'missing'], extension=True, expect=1)
    for command in ('sessions', 'usage'):
        for label, flag in [('short', '-A'), ('long', '--all-accounts')]:
            add('merged-alias-' + command + '-' + label, [command, flag, '--json'], extension=True, merged_inventory=True, check='merged-inventory')
    for label, command, extra in [
        ('url', ['account', 'add', 'work', '--url-only', '--email', 'hint@example.invalid'], {}),
        ('no-use', ['account', 'add', 'work', '--url-only', '--no-use'], {}),
        ('bound', ['account', 'add', 'work', '--url-only', '--drive-account', 'default'], {}),
        ('configure', ['account', 'add', 'work', '--configure-only'], {}),
        ('login-new', ['account', 'login', 'work', '--url-only'], {}),
        ('login-email', ['account', 'login', '--url-only', '--email', 'hint@example.invalid'], {}),
        ('finish-drive-failure', ['account', 'login', '--code', 'compare-code-fixture', '--with-drive'], {'login_finish': True, 'expect': 1}),
        ('unknown-code', ['account', 'login', 'missing', '--code', 'compare-code-fixture'], {'expect': 1}),
        ('invalid-binding', ['account', 'add', 'work', '--url-only', '--drive-account', 'missing'], {'expect': 1}),
        ('conflict', ['account', 'login', '--with-dfs', '--no-dfs'], {'expect': 2}),
    ]:
        add('account-flow-' + label, command, extension=True, check='account-flow-' + label, **extra)
    for shell in ('bash', 'zsh', 'fish', 'powershell', 'pwsh'):
        add('completion-show-' + shell, ['--show-completion', shell], extension=True, check='completion-show')
    add('completion-auto', ['--show-completion'], extension=True, completion_shell='/bin/bash', check='completion-show')
    add('completion-install-bash', ['--install-completion=bash'], extension=True, check='completion-install')
    add('completion-invalid-shell', ['--show-completion=unsupported'], extension=True, expect=2, check='completion-no-write')
    add('completion-conflict', ['--show-completion=bash', '--install-completion=bash'], extension=True, expect=2, check='completion-no-write')
    add('alias-acct-list', ['acct', 'list', '--json'], extension=True)
    add('account-login-code', ['account', 'login', '--code', 'compare-code-fixture'], extension=True, login_finish=True)
    return result


def check_result(case, run, backend, profile, root):
    errors = []
    expected = case.get('expect_' + run['implementation'], case.get('expect', 0))
    code = run['exit_code']
    if code is None or (code == 0 if expected == 'nonzero' else code != expected):
        errors.append(f'exit_code={code}, expected={expected}')
    if 'expect_stdout' in case and run['stdout'] != case['expect_stdout']:
        errors.append('stdout differs from expected fixture output')
    if backend.errors:
        errors.extend(backend.errors)
    if errors or case.get('help'):
        return errors
    check = case.get('check')
    sessions = json.loads((profile / 'sessions.json').read_text())
    if check == 'exec-timeout-diagnostic':
        diagnostic = run['stderr']
        if run['implementation'] == 'rust':
            details = ('超时', '空闲期限 0.2 秒', '可用 --timeout 调整', '执行结果未知', '消息 ID')
        else:
            details = ('TimeoutError', 'no message from the kernel for 0.2s', 'Use --timeout')
        if run['stdout'] or not all(detail in diagnostic for detail in details):
            errors.append('execution timeout diagnostic missing details or appeared on stdout')
    if case['name'] in ['new-default-name', 'new-gpu-tpu-conflict', 'run-default-name', 'ssh-interactive-created']:
        stream = run['stdout'] + run['stderr']
        match = re.search(r"Creating session '([^']+)'", stream)
        pattern = r'run-[0-9a-f]{6}' if case['name'].startswith('run-') else r'[0-9a-f]{6}'
        if not match or not re.fullmatch(pattern, match.group(1)):
            errors.append('automatic name does not follow official format')
        else:
            run['generated_name'] = match.group(1)
    if check and check.startswith('native-update-'):
        expected_wire = ['/native-update.json'] if case.get('native_update_source') or case.get('native_update_env') else []
        if [item['path'] for item in backend.trace] != expected_wire or backend.executed:
            errors.append('native update made unexpected authentication, artifact, or VM requests')
        if any(item['oauth'] or item['proxy'] or item['xsrf'] for item in backend.trace):
            errors.append('native update attached account credentials')
        if (root / 'bin/native-colab').read_bytes() != b'synthetic-native-update-target':
            errors.append('check/no-op/rejected update modified the installation fixture')
        if check in ('native-update-check', 'native-update-env-check') and json.loads(run['stdout']).get('update_available') is not True:
            errors.append('native update lost the newer release')
        if check == 'native-update-up-to-date' and json.loads(run['stdout']).get('status') != 'up_to_date':
            errors.append('already current native update did not remain a no-op')
        if check == 'native-update-status' and json.loads(run['stdout']).get('state') != 'unmanaged':
            errors.append('offline installation status did not describe unmanaged copy')
        if 'private-update-error' in run['stdout'] + run['stderr']:
            errors.append('native update exposed raw release error fields')
    if case.get('inventory_error'):
        value = json.loads(run['stdout'])
        if not isinstance(value, dict) or not value.get('error') or value.get('assignments') != [] or value.get('usage') is not None or run['stderr']:
            errors.append('merged JSON query lost its structured account failure')
    if check and check.startswith('adc-aws-'):
        label = check.removeprefix('adc-aws-')
        expected = []
        if label == 'doctor':
            if json.loads(run['stdout']).get('authentication', {}).get('state') != 'external_account_aws_configured':
                errors.append('AWS doctor lost basic source metadata')
        else:
            if case.get('aws_metadata'):
                expected = ['/latest/api/token']
                if not case.get('aws_v2_failure'):
                    expected += ['/latest/meta-data/iam/security-credentials', '/latest/meta-data/iam/security-credentials/role-fixture']
            if not case.get('aws_v2_failure'):
                expected += ['/v1/token']
                if case.get('external_iam'):
                    expected += ['/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken']
                expected += {'login': [], 'sessions': ['/tun/m/assignments'], 'whoami': ['/tokeninfo'], 'status': ['/tun/m/ccu-info', '/tun/m/assignments'], 'imds': ['/tun/m/assignments']}[label]
        if [item['path'] for item in backend.trace] != expected:
            errors.append('AWS environment/IMDS/STS/IAM/follow-up order mismatch')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['synthetic-aws-secret', 'synthetic-aws-session', 'private-aws-error', 'compare-oauth-fixture', 'impersonated-target-fixture']):
            errors.append('AWS source exposed credential/error material')
    if check and check.startswith('adc-executable-'):
        label = check.removeprefix('adc-executable-')
        expected = []
        if label == 'doctor':
            auth = json.loads(run['stdout']).get('authentication', {})
            if auth.get('state') != 'external_account_executable_configured' or auth.get('executable_policy') != 'enabled':
                errors.append('executable doctor lost configuration or opt-in policy')
        elif label != 'disabled':
            expected = ['/v1/token']
            if case.get('external_iam'):
                expected += ['/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken']
            expected += {'login': [], 'sessions': ['/tun/m/assignments'], 'whoami': ['/tokeninfo'], 'status': ['/tun/m/ccu-info', '/tun/m/assignments']}[label]
        if [item['path'] for item in backend.trace] != expected:
            errors.append('executable source/STS/IAM/follow-up order mismatch')
        if (root / 'executable-ran').exists() != (label not in ('disabled', 'doctor')):
            errors.append('credential plugin ran unexpectedly or did not run when enabled')
        if label == 'status':
            row = json.loads(run['stdout'])[0]
            if row.get('auth_provider') != 'adc' or row.get('error') or row.get('usage', {}).get('balance') != 12.5:
                errors.append('executable ADC status lost IAM identity result')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['external-subject-fixture', 'compare-oauth-fixture', 'impersonated-target-fixture']):
            errors.append('executable ADC exposed subject/access token')
    if check and check.startswith('adc-metadata-'):
        label = check.removeprefix('adc-metadata-')
        expected = []
        if label == 'doctor':
            auth = json.loads(run['stdout']).get('authentication', {})
            if auth.get('state') != 'missing' or auth.get('metadata_fallback') != 'not_probed' or auth.get('online_validity') != 'not_checked':
                errors.append('metadata doctor claimed online identity or lost fallback state')
        elif label == 'spoofed':
            expected = ['/']
        elif label not in ('disabled', 'explicit-missing'):
            expected = ['/', '/computeMetadata/v1/instance/service-accounts/default/token']
            expected += {'login': [], 'sessions': ['/tun/m/assignments'], 'whoami': ['/tokeninfo'], 'status': ['/tun/m/ccu-info', '/tun/m/assignments']}[label]
        if [item['path'] for item in backend.trace] != expected:
            errors.append('metadata fallback or discovery/follow-up order mismatch')
        if label == 'status':
            row = json.loads(run['stdout'])[0]
            if row.get('auth_provider') != 'adc' or row.get('error') or row.get('usage', {}).get('balance') != 12.5:
                errors.append('metadata status lost successful ADC result')
        if 'compare-oauth-fixture' in run['stdout'] + run['stderr']:
            errors.append('metadata command exposed access token')
    if check and check.startswith('adc-workforce-user-'):
        label = check.removeprefix('adc-workforce-user-')
        if label == 'doctor':
            if backend.trace or json.loads(run['stdout']).get('authentication', {}).get('state') != 'external_account_authorized_user_configured':
                errors.append('workforce doctor contacted STS or lost configuration metadata')
        else:
            suffix = {'login': [], 'sessions': ['/tun/m/assignments'], 'whoami': ['/tokeninfo'], 'status': ['/tun/m/ccu-info', '/tun/m/assignments'], 'refresh-failure': []}[label]
            if [item['path'] for item in backend.trace] != ['/v1/oauth/token'] + suffix:
                errors.append('workforce refresh/follow-up order mismatch')
            if label == 'status':
                row = json.loads(run['stdout'])[0]
                if row.get('auth_provider') != 'adc' or row.get('error') or row.get('usage', {}).get('balance') != 12.5:
                    errors.append('workforce status lost successful ADC result')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['workforce-secret', 'workforce-refresh-fixture', 'workforce-rotated-fixture', 'private-workforce-error', 'compare-oauth-fixture']):
            errors.append('workforce ADC exposed credential/error material')
    if check and check.startswith('adc-external-'):
        label = check.removeprefix('adc-external-')
        if label == 'doctor':
            if backend.trace or json.loads(run['stdout']).get('authentication', {}).get('state') != 'external_account_configured':
                errors.append('external doctor evaluated a source or lost configuration metadata')
        else:
            expected = ['/external/subject'] if case.get('external_url') else []
            expected += ['/v1/token']
            if case.get('external_iam'):
                expected += ['/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken']
            expected += {'login': [], 'sessions': ['/tun/m/assignments'], 'whoami': ['/tokeninfo'], 'status': ['/tun/m/ccu-info', '/tun/m/assignments'], 'iam-failure': []}[label]
            if [item['path'] for item in backend.trace] != expected:
                errors.append('external source/STS/IAM/follow-up order mismatch')
            if label == 'status':
                row = json.loads(run['stdout'])[0]
                if row.get('auth_provider') != 'adc' or row.get('error') or row.get('usage', {}).get('balance') != 12.5:
                    errors.append('external ADC status did not use target identity')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['external-subject-fixture', 'external-subject-header', 'impersonated-target-fixture', 'private-iam-error', 'compare-oauth-fixture']):
            errors.append('external ADC exposed credential/error material')
    if check and check.startswith('adc-impersonation-'):
        if check.endswith('-doctor'):
            if backend.trace or json.loads(run['stdout']).get('authentication', {}).get('state') != 'impersonated_service_account_configured':
                errors.append('impersonation doctor contacted a backend or lost basic source metadata')
        else:
            paths = [item['path'] for item in backend.trace]
            iam = '/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken'
            suffix = {'login': [], 'sessions': ['/tun/m/assignments'], 'whoami': ['/tokeninfo'], 'status': ['/tun/m/ccu-info', '/tun/m/assignments'], 'iam-failure': []}[check.removeprefix('adc-impersonation-')]
            if paths != ['/oauth/token', iam] + suffix:
                errors.append('ADC impersonation source/target exchange or follow-up order mismatch')
            for item in backend.trace:
                if item['path'].startswith('/tun/m/') and not item.get('target'):
                    errors.append('Colab did not receive the target token')
            if check.endswith('-status'):
                row = json.loads(run['stdout'])[0]
                if row.get('auth_provider') != 'adc' or row.get('error') or row.get('usage', {}).get('balance') != 12.5:
                    errors.append('impersonation status lost the successful target result')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['impersonated-target-fixture', 'private-iam-error', 'compare-refresh-fixture']):
            errors.append('impersonation command exposed credential/error material')
    if check in ('adc-no-wire', 'adc-doctor', 'adc-status-error'):
        if backend.trace or backend.executed:
            errors.append('ADC rejected source or local doctor contacted a backend')
        if 'synthetic-invalid-private-key' in run['stdout'] + run['stderr']:
            errors.append('ADC error exposed source material')
        if check == 'adc-doctor' and json.loads(run['stdout']).get('authentication', {}).get('state') != 'service_account_configured':
            errors.append('local doctor did not describe service account configuration presence')
        if check == 'adc-status-error':
            row = json.loads(run['stdout'])[0]
            if row.get('auth_provider') != 'adc' or not row.get('error') or row.get('usage') is not None:
                errors.append('ADC account status did not preserve authentication failure as a row')
    if check and check.startswith('completion-'):
        if backend.trace:
            errors.append('completion accessed authentication or backend')
        if check == 'completion-show' and ('colab' not in run['stdout'] or 'account' not in run['stdout']):
            errors.append('completion did not contain the native command tree')
        if check == 'completion-install':
            script = root / '.bash_completions/colab.sh'
            if not script.exists() or 'account' not in script.read_text():
                errors.append('completion installer did not write the script')
            if not (root / '.bashrc').exists() or (root / '.bashrc').read_text().count('# >>> colab native completion >>>') != 1:
                errors.append('completion installer did not create exactly one startup block')
        elif (root / '.bash_completions').exists() or (root / '.bashrc').exists():
            errors.append('completion display or parse failure unexpectedly installed files')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['compare-oauth-fixture', 'compare-refresh-fixture', 'compare-secret-fixture']):
            errors.append('completion exposed credential fixture values')
    if check and check.startswith('account-flow-'):
        label = check.removeprefix('account-flow-')
        registry = json.loads((root / 'config/accounts.json').read_text())
        expected_wire = ['/oauth/token'] if label == 'finish-drive-failure' else []
        if [item['path'] for item in backend.trace] != expected_wire:
            errors.append('account flow made unexpected authorization or control requests')
        active = 'work' if label in ('url', 'bound') else 'default'
        if registry['active'] != active:
            errors.append('account flow switched active at the wrong stage')
        pending_account = 'default' if label == 'login-email' else 'work'
        pending = root / 'config/accounts' / pending_account / 'oauth_pending.json'
        if label in ('url', 'bound', 'no-use', 'login-new', 'login-email'):
            if not pending.exists() or json.loads(pending.read_text())['account'] != pending_account:
                errors.append('account flow did not retain its own PKCE pending')
            else:
                query = parse_qs(urlsplit(run['stdout'].strip()).query)
                if query.get('state') != [json.loads(pending.read_text())['state']]:
                    errors.append('account flow URL does not match its pending state')
                if label in ('url', 'login-email') and query.get('login_hint') != ['hint@example.invalid']:
                    errors.append('account flow lost explicit email hint')
        if label == 'configure' and pending.exists():
            errors.append('configure-only unexpectedly started OAuth')
        if label == 'bound' and registry['accounts']['work'].get('drive_account') != 'default':
            errors.append('account flow lost explicit Drive binding')
        if label in ('invalid-binding', 'unknown-code') and any(name in registry['accounts'] for name in ('work', 'missing')):
            errors.append('failed lookup unexpectedly registered an account')
        if label == 'finish-drive-failure' and ((profile / 'oauth_pending.json').exists() or not (profile / 'oauth_token.json').exists()):
            errors.append('Drive failure lost completed compute login or retained consumed pending')
        if list((root / 'config/accounts').glob('*/dfs_pending.json')) or list((root / 'config/accounts').glob('*/dfs_credentials.json')):
            errors.append('deferred or locally rejected Drive consent wrote Drive credentials')
    if check == 'merged-inventory':
        rows = json.loads(run['stdout'])
        if {row.get('account') for row in rows} != {'default', 'secondary'} or any(row.get('error') for row in rows):
            errors.append('all-accounts alias did not query both configured profiles')
        path = '/tun/m/ccu-info' if case['command'][0] == 'usage' else '/tun/m/assignments'
        if [item['path'] for item in backend.trace] != [path, path]:
            errors.append('all-accounts alias did not issue exactly one query per profile')
    if check == 'account-status':
        if [item['path'] for item in backend.trace] != ['/tun/m/ccu-info', '/tun/m/assignments']:
            errors.append('account status did not query usage then assignments')
        if '--json' in case['command']:
            value = json.loads(run['stdout'])
            if len(value) != 1 or value[0].get('account') != 'default' or value[0].get('active') is not True or value[0].get('usage', {}).get('balance') != 12.5:
                errors.append('account status lost account metadata or usage')
            row = value[0]
            if bool(row.get('error')) != bool(case.get('list_status')) or row.get('drive_ready') != bool(case.get('drive_credentials')):
                errors.append('account status error or Drive presence mismatch')
            expected_names = [] if case.get('list_status') or not case.get('active', True) else ['compare']
            if case.get('orphan'):
                expected_names.append('?')
            if sorted(a['name'] for a in row.get('assignments', [])) != sorted(expected_names):
                errors.append('account status lost named or orphan assignments')
            if case.get('list_status') and 'compare' not in sessions:
                errors.append('account status pruned sessions after a failed query')
        elif '[*default] (-) | CCU: 12.50 (1.25/hr) | sessions: compare:CPU | drive:default(missing)' not in run['stdout']:
            errors.append('account status text format mismatch')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['compare-oauth-fixture', 'compare-proxy-fixture', 'synthetic-drive-refresh']):
            errors.append('account status exposed credentials')
    if check == 'allocated-default' and (len(sessions) != 1 or not backend.active):
        errors.append('default allocation name was not persisted')
    if check == 'allocated' and ('compare' not in sessions or not backend.active):
        errors.append('allocation was not persisted')
    if check in ('utility-pay', 'utility-resource') and backend.trace:
        errors.append('local information command contacted backend')
    if check == 'utility-url':
        paths = ['/tun/m/assignments'] if case['command'] == ['url'] else []
        if [item['path'] for item in backend.trace] != paths:
            errors.append('URL command used an unexpected control-plane request sequence')
        value = run['stdout'].strip().splitlines()[-1]
        parsed = urlsplit(value)
        endpoint = case.get('url_endpoint', 'ep-compare')
        if case['command'] == ['url']:
            origin = 'https://colab.research.google.com'
            backend_path = '/tun/m/' + endpoint
            expected_url = origin + '/notebooks/empty.ipynb?dbu=' + quote(backend_path, safe='') + '#datalabBackendUrl=' + origin + backend_path
            if value != expected_url or [session['endpoint'] for session in sessions.values()] != [endpoint]:
                errors.append('implicit URL or synchronized session state disagrees with selected endpoint')
        if (run['implementation'] == 'rust' and len(run['stdout'].splitlines()) != 1) or parse_qs(parsed.query).get('dbu') != ['/tun/m/' + endpoint] or not parsed.fragment.startswith('datalabBackendUrl='):
            errors.append('browser URL lost query/fragment or pipeable output')
        if case.get('account_email') and parse_qs(parsed.query).get('authuser') != ['compute+test@example.invalid']:
            errors.append('browser URL did not use selected compute email')
    if case.get('browser'):
        capture = root / 'browser-urls.txt'
        if not capture.exists() or len(capture.read_text().splitlines()) != 1:
            errors.append('browser opener did not receive exactly one URL')
    if check == 'utility-resource' and not run['stdout'].strip():
        errors.append('bundled information resource missing')
    if check in ('utility-whoami', 'utility-whoami-error'):
        if [item['path'] for item in backend.trace] != ['/oauth/token', '/tokeninfo']:
            errors.append('identity diagnostic did not force refresh then query tokeninfo')
        if run['implementation'] == 'rust' and ('compare-oauth-fixture' in run['stdout'] + run['stderr'] or 'synthetic-private-tokeninfo' in run['stdout'] + run['stderr']):
            errors.append('identity diagnostic exposed token or raw tokeninfo fields')
        if check == 'utility-whoami' and 'compute@example.invalid' not in run['stdout']:
            errors.append('identity diagnostic lost verified email')
    if check and check.startswith('ext-'):
        if backend.trace or backend.executed:
            errors.append('local extension command contacted the backend')
        if (root / 'python-must-not-create').exists():
            errors.append('native extension install modified Python target')
    if check == 'ext-version' and json.loads(run['stdout']).get('python_required') is not False:
        errors.append('native extension version capability mismatch')
    if check == 'ext-install' and json.loads(run['stdout']).get('changed') is not False:
        errors.append('native extension install claimed an injection write')
    if check == 'ext-quiet' and (run['stdout'] or run['stderr']):
        errors.append('quiet native extension install printed output')
    if check == 'ext-doctor':
        value = json.loads(run['stdout'])
        if value.get('snapshot') != 'read_only_non_atomic' or value.get('registry') != 'ready':
            errors.append('doctor did not report a read-only local snapshot')
        if case.get('token_storage') == 'keyring' and value.get('authentication', {}).get('state') != 'not_inspected':
            errors.append('doctor keyring mode did not remain uninspected')
        if case.get('automation_pending') and not value.get('sessions', [{}])[0].get('operation_pending'):
            errors.append('doctor hid a pending operation')
        for secret in ['compare-oauth-fixture', 'compare-refresh-fixture', 'synthetic-drive-refresh', 'must-not-display']:
            if secret in run['stdout'] + run['stderr']:
                errors.append('doctor exposed credential or unknown metadata')
    if check == 'drivemount-upstream':
        if not any('drive.mount(' in source for source in backend.executed):
            errors.append('official strategy did not execute the Colab mount payload')
        if sessions.get('compare', {}).get('drive_plan') is not None or sessions.get('compare', {}).get('drive_mounts'):
            errors.append('official strategy wrote a native DriveFS plan')
    if check == 'drive-leases-empty':
        if json.loads(run['stdout']) != [] or backend.trace:
            errors.append('empty lease listing changed state or contacted backend')
    if check == 'drive-mount-dry-run':
        value = json.loads(run['stdout'])
        if not value.get('dry_run') or value.get('transport') != 'native' or backend.trace:
            errors.append('mount dry run dispatched a remote request')
    if check == 'drive-empty':
        if not run['stdout'].strip().endswith(': empty (run `consent`)'):
            errors.append('empty Drive credential display changed')
    if check == 'drive-status':
        value = json.loads(run['stdout'])
        expected_drive = 'secondary' if case.get('drive_status_storage') and case['name'] != 'drive-status-account-precedence' else 'default'
        if value.get('account') != 'default' or value.get('drive_account') != expected_drive or value.get('local_credentials_present') != (expected_drive == 'secondary'):
            errors.append('Drive status lost local account selection or credential presence metadata')
        if 'synthetic-private-drive-status-credentials' in run['stdout'] + run['stderr']:
            errors.append('Drive status read or exposed private credential contents')
        if value.get('mounted') is not False or value.get('managed') is not False or value.get('stage') != 'unmanaged':
            errors.append('Drive read-only status result mismatch')
        if sessions['compare'].get('drive_probe') is not None or sessions['compare']['kernel_id'] != 'kernel-compare':
            errors.append('Drive probe cleanup or normal kernel isolation failed')
        if case.get('automation_pending') and sessions['compare'].get('automation_pending', {}).get('kernel_id') != 'automation-owned':
            errors.append('Drive status changed a pending mutation')
        deletes = [item['path'] for item in backend.trace if item['method'] == 'DELETE']
        if deletes != ['/runtime/api/kernels/drive-probe-fixture'] or len(backend.executed) != 1:
            errors.append('Drive status did not use and clean one independent probe kernel')
        if 'synthetic-private-drive-status' in run['stdout'] + run['stderr']:
            errors.append('Drive status leaked an unknown result field')
    if check == 'drive-probe-no-wire' and backend.trace:
        errors.append('Drive preflight or unknown creation made a network request')
    if check in ('drive-probe-empty', 'drive-probe-cleaned'):
        if sessions['compare'].get('drive_probe') is not None:
            errors.append('Drive probe recovery journal was not cleared')
        if check == 'drive-probe-empty' and (backend.trace or json.loads(run['stdout']).get('kernel_id') is not None):
            errors.append('empty probe cleanup changed remote state')
        if check == 'drive-probe-cleaned' and [item['path'] for item in backend.trace] != ['/runtime/api/kernels/drive-probe-fixture']:
            errors.append('probe cleanup touched a foreign kernel')
    if check == 'drive-probe-session-status':
        pending = json.loads(run['stdout'])[0].get('drive_probe', {})
        if pending != {'phase': 'creating', 'kernel_id': None} or 'synthetic-private-drive-probe' in run['stdout']:
            errors.append('session status exposed invalid probe metadata')
    if check == 'drive-summary':
        metadata = json.loads(run['stdout'])
        if metadata.get('user_id') != 'gaia-drive-fixture' or metadata.get('refresh_token') != '<set>':
            errors.append('Drive summary metadata mismatch')
        if any(secret in run['stdout'] + run['stderr'] for secret in ['synthetic-drive-refresh', 'synthetic-drive-client-secret', 'must-not-display']):
            errors.append('Drive summary leaked secret or unknown fields')
    if check == 'drive-cleared' and (profile / 'dfs_credentials.json').exists():
        errors.append('Drive local credentials were not cleared')
    if check == 'drive-token':
        if json.loads(run['stdout']).get('access_token') != 'synthetic-drive-access':
            errors.append('explicit Drive token did not return minted access token')
    if check == 'drive-pending':
        pending = profile / 'dfs_pending.json'
        if not pending.exists():
            errors.append('Drive pending authorization missing')
        else:
            saved = json.loads(pending.read_text())
            query = parse_qs(urlsplit(run['stdout'].strip()).query)
            if saved['account'] != 'default' or saved['state'] != query.get('state', [None])[0] or 'https://www.googleapis.com/auth/drive' not in query.get('scope', [''])[0].split():
                errors.append('Drive URL, scope or state mismatch')
    if check == 'ssh-no-connection':
        if any(item['path'] in ['/colab/ssh', '/tun/m/assign'] for item in backend.trace):
            errors.append('stale SSH child connected or created a replacement runtime')
        if not backend.active or 'compare' not in sessions:
            errors.append('stale SSH child changed existing session state')
    if check in ('ssh-bytes', 'ssh-released'):
        expected_stdout = (b"[colab] Using unique session 'compare'.\n" if case['name'] == 'ssh-proxy-unique' and run['implementation'] == 'official' else b'') + SSH_BYTES
        if run.get('stdout_base64') != base64.b64encode(expected_stdout).decode():
            errors.append('SSH stdout bytes differ or include a banner')
        if check == 'ssh-released' and (sessions or backend.active):
            errors.append('SSH --rm did not release its resource')
    if check == 'ssh-cleanup-difference':
        expected_active = run['implementation'] == 'official'
        if backend.active != expected_active or bool(sessions) != expected_active:
            errors.append('SSH early-failure cleanup behavior changed from baseline')
    if check in ('ssh-command', 'ssh-command-released'):
        marker = root / 'ssh-args'
        if not marker.exists():
            errors.append('OpenSSH subprocess was not invoked')
        else:
            words = marker.read_bytes().split(b'\0')[:-1]
            if words[-1].decode() != case['remote'] or words[-3].decode() != case['pty'] or words[-2] != b'root@colab-runtime':
                errors.append('OpenSSH command argv or PTY flag changed')
        if check == 'ssh-command-released' and (sessions or backend.active):
            errors.append('interactive auto-created VM was not released')
    if check == 'install-executed':
        sources = [code for code in backend.executed if 'def install():' in code]
        if len(sources) != 1:
            errors.append('one installer source was not submitted')
        elif case.get('install_syntax') and run['implementation'] == 'official':
            try:
                ast.parse(sources[0])
                errors.append('quoted official installer unexpectedly parses; update the baseline expectation')
            except SyntaxError:
                if 'SyntaxError' not in run['stderr']:
                    errors.append('official invalid-source result was not surfaced')
        else:
            try:
                tree = ast.parse(sources[0])
                assignment = next(node.value for node in ast.walk(tree) if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == 'packages' for target in node.targets))
                actual = ast.literal_eval(assignment) if isinstance(assignment, ast.List) else json.loads(ast.literal_eval(assignment.args[0]))
                if actual != case['install_args']:
                    errors.append('installer argument vector changed')
            except (ValueError, SyntaxError, StopIteration, AttributeError) as error:
                errors.append('installer arguments could not be decoded: ' + type(error).__name__)
        if case.get('install_file') and backend.files.get(case['install_file']) != b'demo==1.2\n':
            errors.append('requirements upload bytes changed')
    if check == 'automation-cleaned':
        if (sessions['compare'].get('automation_pending') or sessions['compare'].get('running') or sessions['compare'].get('kernel_id') != 'kernel-compare' or not backend.active):
            errors.append('automation cleanup changed ordinary kernel/VM or left pending state')
        deletes = [entry['path'] for entry in backend.trace if entry['method'] == 'DELETE']
        if deletes != ['/runtime/api/kernels/automation-owned']:
            errors.append('cleanup did not delete exactly its owned automation kernel')
    if check == 'automation-refused' and (not sessions['compare'].get('automation_pending') or backend.trace):
        errors.append('conflicting cleanup mutated state or sent a request')
    if check in ('automation-status', 'automation-status-text'):
        if 'must-not-display' in run['stdout'] or 'source_hash' in run['stdout']:
            errors.append('private automation journal fields appeared in status')
        if check == 'automation-status' and json.loads(run['stdout'])[0]['automation_pending']['phase'] != 'unknown':
            errors.append('automation pending state missing from JSON status')
        if check == 'automation-status-text' and 'Automation: unknown' not in run['stdout']:
            errors.append('automation pending state missing from text status')
    if check == 'pruned' and sessions:
        errors.append('stale session was not pruned')
    if check in ('released', 'run-released', 'run-cleaned') and (backend.active or sessions):
        errors.append('resource was not released')
    if check == 'run-cleaned' and list((profile / 'runs').glob('*.json')):
        errors.append('cleaned run intent was not removed')
    if check == 'run-kept' and (not backend.active or 'compare' not in sessions):
        errors.append('run --keep did not retain session')
    if check == 'uploaded-unicode' and backend.files.get('content/中 #?%.bin') != FIXTURE:
        errors.append('unicode path upload bytes differ')
    if check == 'uploaded' and backend.files.get('content/upload.bin') != FIXTURE:
        errors.append('uploaded bytes differ')
    if check == 'image' and (not (root / 'output.png').exists() or not (root / 'output.png').read_bytes().startswith(b'\x89PNG\r\n\x1a\n')):
        errors.append('PNG image was not saved')
    if check == 'downloaded' and (not (root / 'download.bin').exists() or (root / 'download.bin').read_bytes() != FIXTURE):
        errors.append('downloaded bytes differ')
    if check == 'deleted' and 'content/data.bin' in backend.files:
        errors.append('remote file was not deleted')
    if check == 'edited' and backend.files.get('content/edit.txt') != b'after\n':
        errors.append('edited bytes differ')
    if check in ('executed', 'notebook', 'run-released', 'run-kept') and not any('fixture-output' in code for code in backend.executed):
        errors.append('fixture source was not sent over WebSocket')
    if check == 'notebook':
        target = root / 'notebook_output.ipynb'
        if not target.exists() or not json.loads(target.read_text())['cells'][0]['outputs']:
            errors.append('notebook output was not written')
    if check == 'exported':
        target = root / case['command'][-1]
        if not target.exists() or 'fixture-output' not in target.read_text():
            errors.append('history export is missing content')
    if 'compare-oauth-fixture' in run['stdout'] + run['stderr'] or 'compare-proxy-fixture' in run['stdout'] + run['stderr']:
        errors.append('synthetic credential leaked to terminal')
    return errors



def comparable(case, run, key):
    value = run[key]
    generated = run.get('generated_name')
    uuid_pattern = r'[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}'

    def normalized(value, field=None):
        if isinstance(value, dict):
            return {(('<AUTO_NAME>' if k == generated else k) if generated else k): normalized(v, k) for k, v in value.items()}
        if isinstance(value, list):
            return [normalized(v) for v in value]
        if isinstance(value, str):
            if generated and value == generated:
                return '<AUTO_NAME>'
            if ((field in ['id', 'session_id', 'running'] and re.fullmatch(uuid_pattern, value)) or (field == 'session_id' and re.fullmatch(r'[0-9a-f]{8}-[0-9a-f]{24}', value))):
                return '<UUID>'
        return value

    if key in ['stdout', 'stderr'] and generated:
        return value.replace(generated, '<AUTO_NAME>')
    if key in ['artifacts', 'session_state_after']:
        return normalized(value)
    return value

def run_case(args, case, temporary):
    results = {}
    for implementation in ['rust', 'official']:
        if implementation == 'official' and case.get('extension'):
            continue
        root = temporary / case['name'] / implementation
        backend = Backend(case)
        thread = threading.Thread(target=backend.serve_forever, kwargs={'poll_interval': 0.02}, daemon=True)
        thread.start()
        try:
            profile = setup(root, implementation, backend, case)
            env = clean_env(root)
            if case.get('external_aws'):
                env['AWS_DEFAULT_REGION'] = 'invalid-default-region-must-not-use'
                env['AWS_REGION'] = 'us-east-1'
                if case.get('check') == 'adc-aws-sessions':
                    env.pop('AWS_REGION')
                    env['AWS_DEFAULT_REGION'] = 'us-east-1'

                if not case.get('aws_metadata'):
                    env.update(AWS_ACCESS_KEY_ID='AKIDFIXTURE', AWS_SECRET_ACCESS_KEY='synthetic-aws-secret', AWS_SESSION_TOKEN='synthetic-aws-session')

            if case.get('external_executable'):
                env['GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES'] = '0' if case.get('executable_disabled') else '1'

            if case.get('metadata'):
                host = urlsplit(backend.base).netloc
                env.update(GCE_METADATA_HOST=host, GCE_METADATA_IP=host, GCE_METADATA_TIMEOUT='1', NO_GCE_CHECK='true' if case.get('metadata_disabled') else 'false')
                env['GCE_METADATA_ROOT'] = 'http://invalid-root-must-not-use'
                if case.get('check') == 'adc-metadata-sessions':
                    env.pop('GCE_METADATA_HOST')
                    env['GCE_METADATA_ROOT'] = host

                if case.get('metadata_proxy'):
                    env.update(HTTP_PROXY='http://127.0.0.1:1', HTTPS_PROXY='http://127.0.0.1:1', ALL_PROXY='http://127.0.0.1:1', NO_PROXY='')

            if case.get('native_update_env'):
                env['COLAB_CLI_UPDATE_MANIFEST'] = backend.base + '/native-update.json'
            if case.get('native_update_bad_env'):
                env['COLAB_CLI_UPDATE_MANIFEST'] = 'private-invalid-update-channel'
            if case.get('drive_env'):
                env.update(COLAB_DRIVE_REFRESH_TOKEN='synthetic-env-drive-refresh', COLAB_DRIVE_CLIENT_ID='drive-fixture-client',
                           COLAB_DRIVE_CLIENT_SECRET='synthetic-env-drive-secret', COLAB_DRIVE_USER_ID='gaia-drive-fixture',
                           COLAB_DRIVE_SCOPES='https://www.googleapis.com/auth/drive')
            env['EDITOR'] = env['VISUAL'] = str(root / 'editor')
            env['PATH'] = str(root / 'bin') + os.pathsep + env['PATH']
            if case.get('completion_shell'):
                env['SHELL'] = case['completion_shell']
            if case.get('drive_status_env'):
                env['COLAB_DRIVE_ACCOUNT'] = 'secondary'
            if case.get('drive_default'):
                env['COLAB_DFS_DEFAULT' if case.get('legacy_drive_default') else 'COLAB_DRIVE_DEFAULT'] = case['drive_default']
            env['FIXTURE_SSH_ARGUMENTS'] = str(root / 'ssh-args')
            if case.get('browser'):
                env['FIXTURE_BROWSER_URLS'] = str(root / 'browser-urls.txt')
            if implementation == 'rust':
                prefix = [args.rust, '--config-dir', root / 'config', '--token-storage', case.get('token_storage', 'file'), '-c', root / 'client.json', '--control-plane-url', backend.base, '--request-timeout', '3', '--assign-retries', '0']
            else:
                prefix = [args.official_python, '-S', ROOT / 'tools/official_cli_probe.py', '--source', args.official, '--kernel', args.kernel,
                          '--deps', args.deps, '--root', root / 'config', '--base', backend.base, '--']
            if case['name'] == 'drive-token-whoami':
                prefix += ['--drive-userinfo-url', backend.base + '/drive/userinfo']
            if case.get('login_finish'):
                preparation = invoke(prefix + ['account', 'login', '--url-only'], env, root)
                if preparation['exit_code'] != 0:
                    raise RuntimeError('fixture login preparation failed')
            command = case['command']
            if implementation == 'rust' and case.get('native_update_source'):
                command = command + ['--manifest', backend.base + '/native-update.json']
            if implementation == 'rust' and (case['name'].startswith('whoami') or case.get('check') in ('adc-impersonation-whoami', 'adc-external-whoami', 'adc-workforce-user-whoami', 'adc-metadata-whoami', 'adc-executable-whoami', 'adc-aws-whoami')):
                command = command + ['--tokeninfo-url', backend.base + '/tokeninfo']
            if implementation == 'rust' and case.get('native_upstream'):
                command = command + ['--upstream']
            run = invoke(prefix + command, env, root, case.get('stdin', ''), ssh_proxy=case.get('ssh_proxy', False))
            run['implementation'] = implementation
            if implementation == 'rust' and case.get('native_upstream'):
                run['mount_strategy'] = 'explicit_upstream'
            run['checks_failed'] = check_result(case, run, backend, profile, root)
            for key in ['stdout', 'stderr']:
                run[key] = normalize(run[key], root)
            run['wire'] = wire(backend.trace)
            run['executions'] = len(backend.executed)
            saved = json.loads((profile / 'sessions.json').read_text())
            run['session_names_after'] = sorted(saved)
            run['session_state_after'] = {name: {field: state.get(field) for field in ['name', 'endpoint', 'accelerator', 'variant', 'machine_shape', 'kernel_id', 'session_id', 'running']} for name, state in saved.items()}
            run['artifacts'] = {}
            for artifact in ['download.bin', 'output.png', 'notebook_output.ipynb', 'export.jsonl', 'export.md', 'export.txt', 'export.ipynb', 'browser-urls.txt']:
                path = root / artifact
                if path.exists():
                    if path.suffix in ('.bin', '.png'):
                        run['artifacts'][artifact] = {'bytes': path.stat().st_size, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                    elif path.suffix == '.ipynb':
                        run['artifacts'][artifact] = json.loads(path.read_text())
                    else:
                        run['artifacts'][artifact] = normalize(path.read_text(), root)
            run['submitted_code'] = [normalize(code, root) for code in backend.executed]

            run['active_after'] = backend.active
            results[implementation] = run
        finally:
            backend.shutdown()
            backend.server_close()
            thread.join(timeout=2)
    if any(result['checks_failed'] for result in results.values()):
        status = 'failed'
    elif case.get('extension'):
        status = 'rust_extension'
    else:
        rust, official = results['rust'], results['official']
        differences = [key for key in (['exit_code', 'stdout', 'stderr', 'wire', 'session_state_after', 'active_after', 'artifacts'] + (['stdout_base64'] if case.get('ssh_proxy') else [])) if comparable(case, rust, key) != comparable(case, official, key)]
        status = 'different' if differences else 'matched'
    return {'name': case['name'], 'command': case['command'], 'status': status,
            'differences': differences if status in ('different', 'matched') else [], 'results': results}


def validate_sources(args):
    manifest = json.loads((ROOT / 'research/source-inventory.json').read_text())
    metadata = {}
    for name, path in [('cli', args.official), ('kernel', args.kernel)]:
        baseline = manifest['repositories'][name]
        head = subprocess.check_output(['git', '-C', str(path), 'rev-parse', 'HEAD'], text=True).strip()
        if head != baseline['commit']:
            raise SystemExit(f'{name} HEAD does not match pinned baseline')
        for file in baseline['files']:
            if hashlib.sha256((path / file['path']).read_bytes()).hexdigest() != file['sha256']:
                raise SystemExit(f'{name} source hash mismatch: {file["path"]}')
        metadata[name] = {'repository': baseline['repository'], 'commit': head, 'verified_files': len(baseline['files'])}
    return metadata


def markdown(report):
    counts = report['summary']
    lines = ['# CLI 自动对比报告', '', f'场景数：{sum(counts.values())}；分类：' + '、'.join(f'{key}={value}' for key, value in counts.items()) + '。', '',
             '官方源码与 KERNEL 依赖使用 research/source-inventory.json 固定提交并逐文件验哈希。两版使用独立临时配置、合成凭据及同规格 loopback 服务；未访问真实钥匙串、Google、Drive 或 VM。', '',
             '`matched` 仅表示当前合成场景的退出码、输出、请求摘要及会话状态相同；`different` 表示存在已记录差异；`failed` 表示模拟契约断言或测试环境未通过；`rust_extension` 表示官方没有对应命令或参数，不能作为兼容通过。', '',
             '| 场景 | 分类 | 差异 / 未通过检查 |', '| --- | --- | --- |']
    for case in report['cases']:
        details = ', '.join(case['differences'])
        failures = [implementation + ': ' + ', '.join(run['checks_failed']) for implementation, run in case['results'].items() if run['checks_failed']]
        lines.append(f'| {case["name"]} | {case["status"]} | {details or "; ".join(failures) or "—"} |')
    lines += ['', '业务场景分类：' + '、'.join(f'{k}={v}' for k, v in report['behavior_summary'].items()) + '。', '', '自动名称先断言格式，再归一化随机值；Notebook cell ID 与 Jupyter session ID 只在比较时归一化，JSON 中保留原始观察值。', '', '完整 stdout、stderr、HTTP 请求摘要、断言、依赖版本与基线信息见同名 JSON 报告。', '',
              '限制：帮助文本语言与版本号未强行归一化；没有测试真实 OAuth 提供方、OS ACL、Google 配额、真实 kernel 执行、交互 stdin、绘图与长期会话。account rename/remove 的成功和失败恢复路径由 Rust 内存凭据测试覆盖，本脚本只跑无真实钥匙串访问的缺失账户路径。run-cleanup 额外验证已知 endpoint 的所属 run 成功释放与非法 ID 拒绝路径。']
    return '\n'.join(lines) + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/debug/colab')
    parser.add_argument('--official', type=Path, required=True)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--official-python', type=Path, required=True)
    parser.add_argument('--deps', type=Path)
    parser.add_argument('--output', type=Path, default=ROOT / 'research/cli-comparison.json')
    parser.add_argument('--only', help='only run case names matching regex')
    parser.add_argument('--run-rust-tests', action='store_true', help='also run cargo test --locked (temporary/memory fixtures)')
    args = parser.parse_args()
    for name in ['rust', 'official', 'kernel']:
        setattr(args, name, getattr(args, name).resolve())
    args.official_python = args.official_python.absolute()
    if args.deps is None:
        # Preserve the venv entry point; resolving its symlink would lose its installed dependencies.
        python = str(args.official_python)
        args.official_python = Path(python)
        args.deps = Path(subprocess.check_output([python, '-S', '-c', 'import sysconfig; print(sysconfig.get_paths()["purelib"])'], text=True).strip())
    source = validate_sources(args)
    rust_suite = None
    if args.run_rust_tests:
        cargo = shutil.which('cargo')
        if not cargo:
            raise SystemExit('cargo is required for --run-rust-tests')
        result = subprocess.run([cargo, 'test', '--locked'], cwd=ROOT, capture_output=True, text=True, timeout=300)
        output = result.stdout + result.stderr
        rust_suite = {'command': ['cargo', 'test', '--locked'], 'exit_code': result.returncode,
                      'passed': sum(map(int, re.findall(r'test result: ok\. (\d+) passed', output))),
                      'named_tests': re.findall(r'^test ([a-zA-Z0-9_:]+) \.\.\. ok$', output, re.M)}
        if result.returncode:
            failed = re.findall(r'^test ([a-zA-Z0-9_:]+) \.\.\. FAILED$', output, re.M)
            # Keep fixture output/credential bytes out of the comparison report,
            # but preserve the failing test names instead of hiding the cause.
            detail = ': ' + ', '.join(failed) if failed else ' before a test failure was reported'
            raise SystemExit('Rust test suite failed' + detail + '; run cargo test --locked to inspect')
    dependency_script = 'import sys,json,importlib.metadata as m; sys.path.insert(0,sys.argv[1]); print(json.dumps({d.metadata["Name"]:d.version for d in m.distributions(path=[sys.argv[1]])}))'
    dependencies = json.loads(subprocess.check_output([str(args.official_python), '-S', '-c', dependency_script, str(args.deps)], text=True))
    report = {'sources': source, 'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(), 'isolation': {'official_no_site_hooks': True, 'official_network': 'loopback_only', 'credentials': 'synthetic file credentials; real keyring forbidden', 'rust_network': 'fixture URLs only; no real account data'}, 'normalization': ['temporary paths, ports and terminal UUIDs', 'validated six-hex automatic names', 'notebook cell IDs and Jupyter session IDs; raw observations retained'], 'cases': []}
    with tempfile.TemporaryDirectory(prefix='colab-cli-compare-') as directory:
        temporary = Path(directory)
        for case in cases():
            if args.only and not re.search(args.only, case['name']):
                continue
            result = run_case(args, case, temporary)
            report['cases'].append(result)
            print(result['status'] + ' ' + case['name'], flush=True)
    report['summary'] = dict(collections.Counter(case['status'] for case in report['cases']))
    report['behavior_summary'] = dict(collections.Counter(case['status'] for case in report['cases'] if not case['name'].startswith('help-')))
    report['dependency_versions'] = dependencies
    report['rust_regressions'] = rust_suite
    discovered = set()
    groups = {'account': set(), 'drive': set(), 'ext': set()}
    for case in report['cases']:
        if case['name'] == 'help-root':
            discovered = set(re.findall(r'^  ([a-z][a-z-]+)\s', case['results']['rust']['stdout'], re.M))
        for group in groups:
            if case['name'] == 'help-' + group:
                groups[group] = set(re.findall(r'^  ([a-z][a-z-]+)\s', case['results']['rust']['stdout'], re.M)) - {'help'}
    expected = set(COMMON + EXTENSIONS + ['account'])
    report['command_coverage'] = {'discovered': sorted(discovered), 'expected': sorted(expected),
                                'account_subcommands': ACCOUNTS, 'account_discovered': sorted(groups['account']),
                                'drive_subcommands': DRIVES, 'drive_discovered': sorted(groups['drive']),
                                'ext_subcommands': EXT_COMMANDS, 'ext_discovered': sorted(groups['ext']),
                                'unmapped': sorted(discovered - expected)}
    if not args.only and discovered != expected:
        raise SystemExit('Rust command coverage mismatch: ' + repr(discovered ^ expected))
    if not args.only:
        for group, wanted in [('account', set(ACCOUNTS)), ('drive', set(DRIVES)), ('ext', set(EXT_COMMANDS))]:
            if groups[group] != wanted:
                raise SystemExit('Rust ' + group + ' subcommand coverage mismatch: ' + repr(groups[group] ^ wanted))

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    args.output.with_suffix('.md').write_text(markdown(report))
    print(json.dumps(report['summary']))
    raise SystemExit(int(any(case['status'] == 'failed' for case in report['cases'])))


if __name__ == '__main__':
    main()
