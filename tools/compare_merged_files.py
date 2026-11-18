#!/usr/bin/env python3
"""实际合并版文件命令与 Rust 的 loopback/字节对照；仅临时合成账户。"""
import argparse
import base64
from collections import Counter
import hashlib
import json
import math
import os
import random
from pathlib import Path
import subprocess
import sys
import struct
import tempfile
import threading
from urllib.parse import parse_qs, quote, unquote, urlsplit

from capture_merged_help import fingerprint_sources, normalize
from compare_merged_accounts import load_app
from compare_merged_account_mutations import require_no_native_keyring
from compare_merged_inventory import Backend as InventoryBackend, Handler as InventoryHandler, fixture

ROOT = Path(__file__).resolve().parents[1]
REMOTE = 'content/中文 #?% file.bin'
BYTES = b'\x00\xff\x80\nfixture\x00'


def cases():
    return [('list-directory', ['ls', 'content'], 'directory'),
            ('list-file', ['ls', REMOTE], 'base64'),
            ('list-missing', ['ls', REMOTE], 'missing'),
            ('upload-binary', ['upload', 'source.bin', REMOTE], 'upload'),
            ('upload-empty', ['upload', 'empty.bin', REMOTE], 'upload-empty'),
            ('download-binary', ['download', REMOTE, 'result.bin'], 'base64'),
            ('download-empty', ['download', REMOTE, 'result.bin'], 'empty'),
            ('download-text', ['download', REMOTE, 'result.bin'], 'text'),
            ('download-json-unicode', ['download', REMOTE, 'result.bin'], 'json'),
            ('download-json-floats', ['download', REMOTE, 'result.bin'], 'json-floats'),
            ('download-json-large-integers', ['download', REMOTE, 'result.bin'], 'json-integers'),
            ('download-json-escapes', ['download', REMOTE, 'result.bin'], 'json-escapes'),
            ('download-json-float-corpus', ['download', REMOTE, 'result.bin'], 'json-float-corpus'),
            ('download-json-duplicate-keys', ['download', REMOTE, 'result.bin'], 'json-duplicates'),
            ('download-json-number-lexemes', ['download', REMOTE, 'result.bin'], 'json-lexemes'),
            ('download-directory', ['download', 'content', 'result.bin'], 'directory'),
            ('download-missing', ['download', REMOTE, 'result.bin'], 'missing'),
            ('remove', ['rm', REMOTE], 'remove')]


class Backend(InventoryBackend):
    def __init__(self, mode):
        super().__init__({})
        self.RequestHandlerClass = Handler
        self.mode = mode
        self.contents_trace = []


class Handler(InventoryHandler):
    def reply(self, status, data):
        encoded = json.dumps(data).encode()
        if status == 200 and data.get('format') == 'json' and self.server.mode in ('json-duplicates', 'json-lexemes'):
            document = (r'{"a":1,"b":0,"\u0061":2,"nested":[{"c":3,"c":4}]}'
                        if self.server.mode == 'json-duplicates' else '[1.00,1E-0007,-0,-0e0,1e-400,123.4500]')
            encoded = json.dumps(dict(data, content=None)).replace('"content": null', '"content": ' + document).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def contents(self):
        parsed = urlsplit(self.path)
        prefix = '/runtime/work/api/contents/'
        query = parse_qs(parsed.query)
        expected = {'authuser': ['0'], 'colab-runtime-proxy-token': ['proxy-work']}
        if self.server.operation == 'download':
            expected['content'] = ['1']
        if not parsed.path.startswith(prefix) or query != expected or self.headers.get('Authorization'):
            self.server.errors.append('unexpected Contents endpoint, query or compute Authorization')
            self.reply(400, {})
            return
        remote = unquote(parsed.path[len(prefix):])
        if parsed.path != prefix + quote(self.server.expected_remote, safe='/') or remote != self.server.expected_remote:
            self.server.errors.append('remote path encoding mismatch')
        trace = {'method': self.command, 'remote': remote, 'download': query.get('content') == ['1']}
        if self.command == 'PUT':
            length = int(self.headers.get('Content-Length', '0'))
            data = json.loads(self.rfile.read(min(length, 65537)))
            expected_bytes = b'' if self.server.mode == 'upload-empty' else BYTES
            expected_payload = {'name': REMOTE.split('/')[-1], 'path': REMOTE, 'type': 'file', 'format': 'base64',
                                'content': base64.b64encode(expected_bytes).decode(), 'chunk': 1}
            if data != expected_payload:
                self.server.errors.append('unexpected upload payload or bytes')
            trace['payload'] = data
        self.server.contents_trace.append(trace)
        if self.server.mode == 'missing':
            self.reply(404, {})
        elif self.command == 'DELETE':
            self.send_response(204)
            self.end_headers()
        else:
            content = base64.b64encode(BYTES).decode()
            fmt = 'base64'
            if self.server.mode == 'text':
                content, fmt = '中文 text\n', 'text'
            elif self.server.mode == 'json':
                content, fmt = {'中文': ['💻', True, None, 42], 'nested': {'é': 'ok', 'del': '\x7f'}}, 'json'
            elif self.server.mode == 'json-floats':
                content, fmt = [-0.0, 1e-7, 1e-5, 0.0001, 1e15, 1e16, 1e20, 1e308, 5e-324,
                                1.2345678901234567, 2.2250738585072014e-308], 'json'
            elif self.server.mode == 'json-integers':
                content, fmt = [2**64, -(2**63)-1, 10**100, 2**4000, {'id': 123456789012345678901234567890}], 'json'
            elif self.server.mode == 'json-escapes':
                content, fmt = {'literal': r'\u00e9', 'escaped': '\\"\b\t\n\r\f\x00/',
                                'separators': '\u2028\u2029', 'nested': [[], {}, ['é']],
                                '$serde_json::private::Number': '123', '$serde_json::private::RawValue': 'true'}, 'json'
            elif self.server.mode == 'json-float-corpus':
                rng = random.Random(20261118)
                content = []
                while len(content) < 1024:
                    value = struct.unpack('>d', rng.getrandbits(64).to_bytes(8, 'big'))[0]
                    if math.isfinite(value):
                        content.append(value)
                fmt = 'json'
            elif self.server.mode in ('json-duplicates', 'json-lexemes'):
                content, fmt = None, 'json'
            elif self.server.mode == 'empty':
                content = ''
            kind = 'directory' if self.server.mode == 'directory' else 'file'
            if kind == 'directory':
                content = [{'name': name, 'path': 'content/' + name, 'type': kind} for name, kind in
                           [('z.txt', 'file'), ('b', 'directory'), ('a.txt', 'file'), ('a', 'directory')]]
            self.reply(200, {'name': remote.split('/')[-1], 'path': remote, 'type': kind, 'format': fmt, 'content': content})

    def do_GET(self):
        if self.path.startswith('/runtime/'):
            self.contents()
        else:
            super().do_GET()

    def do_PUT(self):
        self.contents()

    def do_DELETE(self):
        self.contents()


def child(args):
    app, violations = load_app(args, loopback_origin=args.base)
    import requests
    original = requests.Session.__init__
    def session_init(self, *values, **kwargs):
        original(self, *values, **kwargs)
        self.trust_env = False
    requests.Session.__init__ = session_init
    from colab_cli import common
    from colab_cli.client import Prod
    common.Prod = lambda: Prod(domain=args.base, api=args.base)
    from typer.testing import CliRunner
    import importlib.metadata
    result = CliRunner().invoke(app, json.loads(args.arguments), prog_name='colab', color=False)
    print(json.dumps({'exit_code': result.exit_code, 'stdout': result.stdout, 'stderr': result.stderr,
                      'forbidden_operations': violations, 'python_version': sys.version.split()[0],
                      'dependency_versions': {name: importlib.metadata.version(name) for name in ('google-auth', 'requests', 'typer', 'pydantic')}}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('cli', 'ext', 'kernel', 'python', 'deps'):
        parser.add_argument('--' + key, type=Path, required=key in ('cli', 'ext', 'kernel'))
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/file-comparison/debug/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-files-comparison.json')
    parser.add_argument('--child', action='store_true')
    parser.add_argument('--base')
    parser.add_argument('--arguments')
    args = parser.parse_args()
    if args.child:
        return child(args)
    if not args.python:
        parser.error('--python is required')
    require_no_native_keyring(args.rust)
    args.deps = Path(subprocess.check_output([str(args.python), '-S', '-c', 'import sysconfig; print(sysconfig.get_path("purelib"))'], text=True).strip())
    records = []
    runtime = None
    for name, arguments, mode in cases():
        row = {'name': name, 'arguments': arguments}
        for implementation in ('official', 'rust'):
            server = Backend(mode)
            server.operation = arguments[0]
            server.expected_remote = arguments[2] if arguments[0] == 'upload' else arguments[1]
            thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.02}, daemon=True)
            thread.start()
            try:
                with tempfile.TemporaryDirectory(prefix='merged-files-') as directory:
                    home = Path(directory).resolve()
                    fixture(home, server.base, {})
                    (home / 'source.bin').write_bytes(BYTES)
                    (home / 'empty.bin').write_bytes(b'')
                    (home / 'result.bin').write_bytes(b'original-local-sentinel')
                    protected = {p: p.read_bytes() for p in (home / 'config').rglob('*')
                                 if p.is_file() and p.name in ('accounts.json', 'oauth_token.json', 'dfs_credentials.json')}
                    for path in (home / 'source.bin', home / 'empty.bin', home / 'config/accounts/default/sessions.json'):
                        protected[path] = path.read_bytes()
                    env = {'HOME': str(home), 'PATH': os.defpath, 'LANG': 'C.UTF-8', 'COLAB_TOKEN_STORAGE': 'file',
                           'COLAB_CLI_CONFIG_DIR': str(home / 'config'), 'NO_COLOR': '1', 'TERM': 'dumb'}
                    cli_args = ['-a', 'work', *arguments, '-s', 'train']
                    if implementation == 'official':
                        command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--deps', str(args.deps),
                                   '--base', server.base, '--arguments', json.dumps(cli_args)]
                        for key in ('cli', 'ext', 'kernel'):
                            command += ['--' + key, str(getattr(args, key).resolve())]
                    else:
                        command = [str(args.rust.resolve()), '--token-storage', 'file', '--control-plane-url', server.base,
                                   '--request-timeout', '2', *cli_args]
                    result = subprocess.run(command, env=env, cwd=home, capture_output=True, text=True, timeout=30)
                    if implementation == 'official':
                        assert result.returncode == 0, 'official probe child failed'
                        observed = json.loads(result.stdout)
                        assert not observed['forbidden_operations']
                        versions = {key: observed[key] for key in ('python_version', 'dependency_versions')}
                        assert runtime is None or runtime == versions, 'oracle dependency versions changed'
                        runtime = versions
                    else:
                        observed = {'exit_code': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
                    for secret in ('fixture-bearer', 'fixture-refresh', 'fixture-secret', 'proxy-work'):
                        assert secret not in observed['stdout'] + observed['stderr'], 'fixture credential leaked'
                    failed = mode == 'missing' or (mode == 'directory' and arguments[0] == 'download')
                    assert observed['exit_code'] == int(failed), 'unexpected command outcome'
                    downloaded = (home / 'result.bin').read_bytes()
                    if arguments[0] != 'download' or failed:
                        assert downloaded == b'original-local-sentinel', 'failed/unrelated command changed local destination'
                    elif mode in ('base64', 'empty', 'text'):
                        expected_bytes = {'base64': BYTES, 'empty': b'', 'text': '中文 text\n'.encode()}[mode]
                        assert downloaded == expected_bytes, 'downloaded bytes differ from response fixture'
                    row[implementation] = {key: normalize(observed[key], home).replace(server.base, '<LOOPBACK>')
                                           if isinstance(observed[key], str) else observed[key] for key in ('exit_code', 'stdout', 'stderr')}
                    row[implementation]['result_sha256'] = hashlib.sha256((home / 'result.bin').read_bytes()).hexdigest()
                    row[implementation]['requests'] = server.contents_trace
                    assert all(p.read_bytes() == original for p, original in protected.items()), 'credential/registry changed'
                    assert not server.errors, server.errors
                    assert server.trace == [{'account': 'work', 'path': '/tun/m/assignments'}], 'unexpected compute account request'
                    assert len(server.contents_trace) == 1, 'missing or duplicate Contents request'
                    assert server.contents_trace[0]['method'] == {'ls': 'GET', 'download': 'GET', 'upload': 'PUT', 'rm': 'DELETE'}[arguments[0]]
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=3)
        row['status'] = 'matched' if row['official'] == row['rust'] else 'unexpected_difference'
        records.append(row)
    report = {**runtime, 'scope': 'actual merged file commands with synthetic cached OAuth, exact loopback origin only; no real credentials or cloud resources',
              'sources': fingerprint_sources({key: getattr(args, key) for key in ('cli', 'ext', 'kernel')}),
              'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(), 'native_keyring_compiled': False,
              'json_numeric_samples': {'finite_binary64_corpus': 1024, 'seed': 20261118,
                                      'largest_integer_bits': 4001, 'duplicate_keys': True,
                                      'noncanonical_number_lexemes': True},
              'normalization': ['temporary HOME', 'exact loopback origin'], 'cases': records,
              'summary': {'cases': len(records), **dict(Counter(row['status'] for row in records))}}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(report['summary']))
    if any(row['status'] == 'unexpected_difference' for row in records):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
