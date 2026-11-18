#!/usr/bin/env python3
"""Opt-in CPU capture. Raw responses stay private; only fixed probe output is published."""
import argparse
import ast
import base64
import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import stat
import re
import uuid

from validate_live_ssh import Client, ValidationError, capture_allocation, cleanup, private_json

ROOT = Path(__file__).resolve().parents[1]
TEXT = {'name': 'stdout', 'text': 'COLABCLI_CODEC_TEXT\n'}
BINARY = {'data': {'text/plain': 'COLABCLI_CODEC_BINARY'}, 'metadata': {}}
BUFFERS = [bytes([0, 255, 128, 65]), b'']


def official_functions(kernel):
    baseline = json.loads((ROOT / 'research/source-inventory.json').read_text())['repositories']['kernel']
    name = 'jupyter_kernel_client/utils.py'
    source = (kernel / name).read_bytes()
    digest = next(row['sha256'] for row in baseline['files'] if row['path'] == name)
    if hashlib.sha256(source).hexdigest() != digest:
        raise ValidationError('official_source_mismatch')
    names = {'deserialize_msg_from_ws_default', 'serialize_msg_to_ws_default', 'serialize_msg_to_ws_json'}
    functions = [n for n in ast.parse(source).body if isinstance(n, ast.FunctionDef) and n.name in names]
    if {f.name for f in functions} != names:
        raise ValidationError('official_functions_missing')
    namespace = {'json': json}
    exec(compile(ast.Module(body=functions, type_ignores=[]), name, 'exec'), namespace)
    return namespace, {'commit': baseline['commit'], 'path': name, 'sha256': digest}


def sanitize_capture(captured, functions):
    if captured.get('passed') is not True or len(captured.get('samples', [])) != 2:
        raise ValidationError('capture_incomplete')
    output, seen = [], set()
    for sample in captured['samples']:
        kind = sample['wire_type']
        raw = base64.b64decode(sample['wire_base64'], validate=True)
        if len(raw) > 8 * 1024 * 1024 or kind not in ('text', 'binary'):
            raise ValidationError('capture_invalid_frame')
        decoded = functions['deserialize_msg_from_ws_default'](raw.decode() if kind == 'text' else raw)
        buffers = decoded.pop('buffers', [])
        expected = copy.deepcopy(sample['expected_envelope'])
        inline_buffers = expected.pop('buffers', [])
        if inline_buffers != []:
            raise ValidationError('unexpected_inline_buffers')
        if decoded != expected or [base64.b64encode(b).decode() for b in buffers] != sample['buffers_base64']:
            raise ValidationError('raw_codec_disagreement')
        msg_type, content = decoded['header']['msg_type'], decoded['content']
        version = decoded['header'].get('version')
        if not isinstance(version, str) or not re.fullmatch(r'[0-9]{1,3}\.[0-9]{1,3}', version):
            raise ValidationError('unexpected_protocol_version')
        if decoded['channel'] != 'iopub':
            raise ValidationError('unexpected_capture_channel')
        if kind == 'text' and msg_type == 'stream' and content == TEXT and buffers == []:
            label = 'text'
        elif kind == 'binary' and msg_type == 'display_data' and content == BINARY and buffers == BUFFERS:
            label = 'binary'
        else:
            raise ValidationError('unexpected_capture_content')
        if label in seen:
            raise ValidationError('duplicate_capture_kind')
        seen.add(label)
        # This is a derived, reencoded fixture. Real identities/metadata never
        # leave the private capture; the fixed content and raw buffers remain.
        envelope = {'header': {'msg_id': 'fixture-' + label, 'session': 'fixture-session',
            'username': 'fixture', 'date': '2026-11-18T00:00:00Z', 'version': version, 'msg_type': msg_type},
            'parent_header': {'msg_id': 'fixture-request'}, 'metadata': {}, 'content': copy.deepcopy(content), 'channel': 'iopub'}
        message = copy.deepcopy(envelope)
        if buffers:
            message['buffers'] = buffers
        wire = (functions['serialize_msg_to_ws_json'](message).encode() if kind == 'text'
                else functions['serialize_msg_to_ws_default'](message))
        output.append({'wire_type': kind, 'wire_base64': base64.b64encode(wire).decode(),
            'expected_envelope': envelope, 'buffers_base64': [base64.b64encode(b).decode() for b in buffers]})
    if seen != {'text', 'binary'}:
        raise ValidationError('missing_capture_kind')
    return {'schema_version': 1, 'source': 'derived from real Colab DEFAULT frames',
        'sanitization': 'headers replaced, metadata/convenience fields removed, JSON and offsets reencoded; fixed content and buffers unchanged',
        'raw_rust_and_official_decoders_agreed': True, 'samples': output}


class TerminalClient(Client):
    """Keep the caller's controlling terminal for native keychain interaction."""
    def call(self, args, data=None, timeout=60):
        return subprocess.run(self.prefix + ['--keyring-timeout', '120'] + args,
            input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env,
            timeout=max(timeout, 180))


def inventory_before_allocation(client, ledger, save):
    ledger['baseline_endpoints'] = sorted(client.inventory())
    save()


def local_owned_state(root, ledger):
    states = json.loads((root / 'accounts' / ledger['account'] / 'sessions.json').read_text())
    state = states.get(ledger['session'])
    if not isinstance(state, dict) or state.get('endpoint') != ledger['endpoint']:
        raise ValidationError('runtime_endpoint_replaced')
    return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/colab')
    parser.add_argument('--probe', type=Path, default=ROOT / 'target/debug/examples/live_codec_probe')
    parser.add_argument('--kernel', type=Path)
    parser.add_argument('--config-dir', type=Path, required=True)
    parser.add_argument('--account', required=True)
    parser.add_argument('--auth', choices=('oauth2', 'adc'), default='oauth2')
    parser.add_argument('--token-storage', choices=('keyring', 'auto', 'file'), default='keyring')
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--cleanup-only', action='store_true')
    args = parser.parse_args()
    if not args.cleanup_only and args.kernel is None:
        parser.error('kernel source required for capture/preflight')
    root, directory = args.config_dir.resolve(), args.state_dir.resolve()
    if args.state_dir.is_symlink():
        parser.error('symlink state refused')
    if os.name != 'posix' or (args.execute and args.cleanup_only) or directory.is_relative_to(root):
        parser.error('Unix/private state outside config required; conflicting actions')
    if not args.cleanup_only:
        directory.mkdir(mode=0o700)
    if not directory.is_dir() or directory.stat().st_uid != os.getuid() or directory.stat().st_mode & 0o077:
        parser.error('private ordinary state directory required')
    lock = os.open(directory / 'lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    info = os.fstat(lock)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1:
        os.close(lock)
        parser.error('unsafe lock')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    binary, probe = args.binary.resolve(), args.probe.resolve()
    hashes = {'binary': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'probe': None if args.cleanup_only else hashlib.sha256(probe.read_bytes()).hexdigest()}
    env = {k: v for k, v in os.environ.items() if not k.startswith('COLAB_')}
    if args.cleanup_only:
        ledger = json.loads((directory / 'ledger.json').read_text())
        if ledger['config_dir'] != str(root) or ledger['account'] != args.account or ledger['auth'] != args.auth or ledger['token_storage'] != args.token_storage:
            parser.error('cleanup identity differs')
    else:
        ledger = {'config_dir': str(root), 'account': args.account, 'auth': args.auth,
            'token_storage': args.token_storage, 'session': 'codec-validation-' + uuid.uuid4().hex[:16],
            'baseline_endpoints': [], 'allocation_requested': False, 'endpoint': None}
    client = TerminalClient(binary, root, args.account, args.auth, args.token_storage, env)
    report = {'schema_version': 1, 'passed': False, 'binary_sha256': hashes['binary'], 'probe_sha256': hashes['probe'],
        'created_sessions': 0, 'cleanup': 'not_allocated', 'execute_requested': args.execute, 'cleanup_only': args.cleanup_only}
    save = lambda: private_json(directory / 'ledger.json', ledger)
    def call(arguments, data=None):
        if hashes['binary'] != hashlib.sha256(binary.read_bytes()).hexdigest():
            raise ValidationError('binary_changed')
        result = client.call(arguments, data=data)
        if result.returncode:
            raise ValidationError('native_command_failed')
        return result
    save()
    try:
        if not args.cleanup_only:
            functions, source = official_functions(args.kernel)
            report['official_source'] = source
            if (root / 'accounts' / args.account / 'control_operation.json').exists():
                raise ValidationError('existing_control_operation')
            inventory_before_allocation(client, ledger, save)
            report['credentials_ready'] = True
            if args.execute:
                if ledger['session'] in json.loads((root / 'accounts' / args.account / 'sessions.json').read_text()):
                    raise ValidationError('session_name_collision')
                ledger['allocation_requested'] = True
                save()
                allocated = call(['new', '-s', ledger['session'], '--json'])
                capture_allocation(json.loads(allocated.stdout), ledger)
                save()
                report['created_sessions'] = 1
                if not ledger['is_cpu']:
                    raise ValidationError('non_cpu_assignment')
                prefix = ['--colab-runtime-endpoint', ledger['endpoint']]
                initialized = json.loads(call(prefix + ['exec', '-s', ledger['session'], '--json'], b'pass\n').stdout)
                if not initialized or any(row.get('status') != 'succeeded' for row in initialized):
                    raise ValidationError('initialization_failed')
                state = local_owned_state(root, ledger)
                if not state.get('kernel_id'):
                    raise ValidationError('kernel_not_recorded')
                ledger['probe_requested'] = True
                save()
                if hashes['probe'] != hashlib.sha256(probe.read_bytes()).hexdigest():
                    raise ValidationError('probe_changed')
                result = subprocess.run([str(probe), '--capture'], input=json.dumps({'state': state,
                    'expected_endpoint': ledger['endpoint'], 'expected_kernel': state['kernel_id']}).encode(),
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=100)
                if result.returncode:
                    raise ValidationError('capture_failed')
                captured = json.loads(result.stdout)
                private_json(directory / 'raw-capture.json', captured)
                fixtures = sanitize_capture(captured, functions)
                replay = subprocess.run([str(probe), '--replay'], input=json.dumps(fixtures).encode(),
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=10)
                if replay.returncode or json.loads(replay.stdout).get('replayed_samples') != 2:
                    raise ValidationError('sanitized_replay_failed')
                private_json(directory / 'sanitized-samples.json', fixtures)
                report.update(raw_codecs_agreed=True, sanitized_samples_replayed=2,
                    actual_non_utf8_buffer_verified=True, actual_empty_buffer_verified=True,
                    reply_and_idle_verified=captured['reply_seen'] and captured['idle_seen'])
            else:
                report['preflight_only'] = True
        report['passed'] = True
    except Exception as error:
        report['error_class'] = str(error) if isinstance(error, ValidationError) else type(error).__name__
    finally:
        if ledger['allocation_requested']:
            try:
                report['cleanup'] = cleanup(client, ledger)
                report['other_new_assignment_count'] = len(ledger.get('other_new_endpoints', []))
            except Exception:
                report['cleanup'] = 'pending_inventory_error'
            save()
            if report['cleanup'] != 'confirmed_released':
                report['passed'] = False
        private_json(directory / 'report.json', report)
        print(json.dumps(report))
        os.close(lock)
    if not report['passed']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
