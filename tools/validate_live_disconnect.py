#!/usr/bin/env python3
"""Opt-in CPU execution disconnect test using an opaque loopback CONNECT relay."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import threading
import time
from urllib.parse import urlsplit
import uuid

from validate_live_ssh import Client, ValidationError, capture_allocation, cleanup, private_json, terminate_group
from validate_live_codec import TerminalClient


class CutRelay:
    """Forward only one allowed authority; never inspect tunneled TLS bytes."""
    def __init__(self, host, port=443):
        self.authority = (host.lower(), port)
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen()
        self.listener.settimeout(.1)
        self.url = 'http://127.0.0.1:' + str(self.listener.getsockname()[1])
        self.lock = threading.Lock()
        self.sockets = set()
        self.workers = []
        self.stopping = threading.Event()
        self.cut_done = False
        self.tunnels = 0
        self.after_cut_tunnels = 0
        self.rejected = 0
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopping.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            with self.lock:
                self.sockets.add(connection)
            worker = threading.Thread(target=self.relay, args=(connection,), daemon=True)
            self.workers.append(worker)
            worker.start()

    def relay(self, connection):
        upstream = None
        try:
            connection.settimeout(10)
            header = b''
            while b'\r\n\r\n' not in header:
                chunk = connection.recv(4096)
                if not chunk:
                    return
                header += chunk
                if len(header) > 16384:
                    return
            head, remainder = header.split(b'\r\n\r\n', 1)
            method, target, version = head.split(b'\r\n', 1)[0].decode('ascii').split()
            parsed = urlsplit('//' + target)
            if (method != 'CONNECT' or version != 'HTTP/1.1' or parsed.username or parsed.password
                    or parsed.path or parsed.query or parsed.fragment
                    or (parsed.hostname, parsed.port) != self.authority):
                with self.lock:
                    self.rejected += 1
                connection.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n')
                return
            upstream = socket.create_connection(self.authority, timeout=10)
            with self.lock:
                self.sockets.add(upstream)
                self.tunnels += 1
                if self.cut_done:
                    self.after_cut_tunnels += 1
            connection.sendall(b'HTTP/1.1 200 Connection Established\r\n\r\n')
            if remainder:
                upstream.sendall(remainder)
            connection.settimeout(1)
            upstream.settimeout(1)
            while not self.stopping.is_set():
                ready, _, _ = select.select([connection, upstream], [], [], .1)
                for source in ready:
                    chunk = source.recv(16384)
                    if not chunk:
                        return
                    (upstream if source is connection else connection).sendall(chunk)
        except (OSError, ValueError):
            pass
        finally:
            for stream in (connection, upstream):
                if stream is not None:
                    with self.lock:
                        self.sockets.discard(stream)
                    stream.close()

    def cut(self):
        with self.lock:
            sockets = list(self.sockets)
            self.cut_done = True
        for stream in sockets:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        return len(sockets)

    def close(self):
        self.stopping.set()
        self.listener.close()
        self.cut()
        self.thread.join(timeout=2)
        for worker in self.workers:
            worker.join(timeout=11)


def save_result(path, result):
    private_json(path, {'exit_code': result.returncode,
                       'stdout_base64': base64.b64encode(result.stdout).decode(),
                       'stderr_base64': base64.b64encode(result.stderr).decode()})


def session_state(root, account, name, endpoint):
    state = json.loads((root / 'accounts' / account / 'sessions.json').read_text())[name]
    if state['endpoint'] != endpoint:
        raise ValidationError('owned_session_replaced')
    return state


def verify_disconnect_history(root, account, name, pending, marker):
    path = (root / 'accounts' / account / 'history/rust-v1'
            / (hashlib.sha256(name.encode()).hexdigest() + '.jsonl'))
    events = [json.loads(line) for line in path.read_text().splitlines()]
    matches = [event for event in events if event.get('event_type') == 'execution'
               and event.get('msg_id') == pending]
    if len(matches) != 1 or matches[0].get('status') != 'unknown':
        raise ValidationError('missing_durable_unknown_execution')
    event = matches[0]
    causes = {'执行完成前连接丢失': 'connection_lost', '执行完成前连接关闭': 'connection_closed'}
    if event.get('error') not in causes:
        raise ValidationError('unknown_cause_is_not_transport_disconnect')
    if not any(output.get('output_type') == 'stream' and marker in output.get('text', '').splitlines()
               for output in event.get('outputs', [])):
        raise ValidationError('partial_remote_output_not_retained')
    return causes[event['error']]


def validate(client, root, ledger, directory, report):
    name, endpoint = ledger['session'], ledger['endpoint']
    current = lambda: session_state(root, ledger['account'], name, endpoint)
    host = urlsplit(current()['url']).hostname
    if not host:
        raise ValidationError('missing_runtime_host')
    marker = 'COLAB_CUT_' + uuid.uuid4().hex
    remote = 'content/colabcli-cut-' + uuid.uuid4().hex + '.txt'
    script = directory / 'job.py'
    script.write_text('from pathlib import Path\nimport time\n'
                      'path=Path(' + repr('/' + remote) + ')\n'
                      'with path.open("a") as f: f.write("once\\n")\n'
                      'print(' + repr(marker) + ',flush=True)\ntime.sleep(4)\n'
                      'with path.open("a") as f: f.write("done\\n")\n')
    relay = CutRelay(host)
    process = None
    try:
        env = {k: v for k, v in client.env.items()
               if k.lower() not in ('https_proxy', 'http_proxy', 'all_proxy', 'no_proxy')}
        env['HTTPS_PROXY'] = relay.url
        env['HTTP_PROXY'] = relay.url
        with open(directory / 'exec-stdout', 'wb') as out, open(directory / 'exec-stderr', 'wb') as err:
            os.chmod(directory / 'exec-stdout', 0o600)
            os.chmod(directory / 'exec-stderr', 0o600)
            process = subprocess.Popen(client.prefix + ['exec', '-s', name, '--timeout', '30', '-f', str(script)],
                                       stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                       env=env, start_new_session=True)
            deadline = time.monotonic() + 90
            while marker.encode() not in (directory / 'exec-stdout').read_bytes().splitlines():
                if process.poll() is not None:
                    raise ValidationError('exec_exited_before_remote_marker')
                if time.monotonic() >= deadline:
                    raise ValidationError('remote_marker_timeout')
                time.sleep(.05)
            if relay.tunnels == 0:
                raise ValidationError('execution_bypassed_fault_relay')
            if relay.cut() == 0:
                raise ValidationError('no_live_tunnel_to_cut')
            report['fault_injected_after_remote_marker'] = True
            process.wait(timeout=30)
        error = (directory / 'exec-stderr').read_text()
        state = current()
        pending = state.get('running') or state.get('execution_pending')
        if process.returncode == 0 or '执行结果未知' not in error or not pending:
            raise ValidationError('disconnect_not_reported_and_persisted_unknown')
        report['disconnect_cause'] = verify_disconnect_history(root, ledger['account'], name, pending, marker)
        if relay.after_cut_tunnels:
            raise ValidationError('unexpected_reconnect_after_cut')
        report['checks'].append({'name': 'disconnect_reports_and_persists_unknown', 'passed': True})
        report['checks'].append({'name': 'transport_disconnect_reason_and_partial_output_retained', 'passed': True})
        report['checks'].append({'name': 'no_automatic_tunnel_reconnect', 'passed': True})
    finally:
        if process is not None and process.poll() is None:
            terminate_group(process)
        relay.close()
        report['relay_tunnels_before_cut'] = relay.tunnels - relay.after_cut_tunnels
        report['relay_tunnels_after_cut'] = relay.after_cut_tunnels
        report['relay_rejected_requests'] = relay.rejected

    blocked = client.call(['exec', '-s', name, '--json', '-f', str(script)])
    save_result(directory / 'blocked-exec.json', blocked)
    if blocked.returncode == 0 or '上次执行尚未确认结束' not in blocked.stderr.decode():
        raise ValidationError('pending_execution_was_not_blocked')
    report['checks'].append({'name': 'subsequent_exec_blocked_until_explicit_recovery', 'passed': True})
    target = directory / 'remote-counter.txt'
    deadline = time.monotonic() + 30
    while True:
        downloaded = client.call(['download', '-s', name, remote, str(target)])
        save_result(directory / 'download.json', downloaded)
        if downloaded.returncode == 0 and target.read_text() == 'once\ndone\n':
            break
        if downloaded.returncode or (target.exists() and target.read_text() not in ('once\n', 'once\ndone\n')):
            raise ValidationError('remote_side_effect_counter_not_exactly_once')
        if time.monotonic() > deadline:
            raise ValidationError('remote_completion_not_observed')
        time.sleep(.25)
    report['checks'].append({'name': 'remote_continued_once_without_code_replay', 'passed': True})
    restarted = client.call(['restart-kernel', '-s', name])
    save_result(directory / 'restart.json', restarted)
    if restarted.returncode or current().get('running') or current().get('execution_pending'):
        raise ValidationError('explicit_restart_did_not_clear_pending')
    verified = client.call(['exec', '-s', name, '--json'],
                           data=('from pathlib import Path\nassert Path(' + repr('/' + remote)
                                 + ').read_text()=="once\\ndone\\n"\nprint("recovery_verified")\n').encode())
    save_result(directory / 'recovered-exec.json', verified)
    results = json.loads(verified.stdout) if verified.returncode == 0 else []
    if not results or any(row.get('status') != 'succeeded' for row in results):
        raise ValidationError('execution_after_explicit_restart_failed')
    report['checks'].append({'name': 'explicit_restart_recovers_execution_without_replaying_script', 'passed': True})


def validate_kernel_death(client, root, ledger, directory, report):
    name, endpoint = ledger['session'], ledger['endpoint']
    bound = ['--colab-runtime-endpoint', endpoint]
    current = lambda: session_state(root, ledger['account'], name, endpoint)
    ready = client.call(bound + ['exec', '-s', name, '--json'], data=b'pass\n')
    ready_values = json.loads(ready.stdout) if ready.returncode == 0 else []
    if not ready_values or any(row.get('status') != 'succeeded' for row in ready_values):
        raise ValidationError('kernel_death_warmup_failed')
    report['kernel_warmup_verified'] = True
    report['stdout_grace_seconds'] = 1
    marker = 'COLAB_DEATH_' + uuid.uuid4().hex
    remote = 'content/colabcli-death-' + uuid.uuid4().hex + '.txt'
    script = directory / 'job.py'
    script.write_text('from pathlib import Path\nimport os,time\n'
                      'with Path(' + repr('/' + remote) + ').open("a") as f:f.write("once\\n")\n'
                      'print(' + repr(marker) + ',flush=True)\ntime.sleep(1)\nos._exit(7)\n')
    result = client.call(bound + ['exec', '-s', name, '--json', '--timeout', '30', '-f', str(script)], timeout=75)
    save_result(directory / 'death-exec.json', result)
    values = json.loads(result.stdout)
    if result.returncode == 0 or len(values) != 1 or values[0].get('status') != 'unknown':
        raise ValidationError('kernel_death_not_unknown')
    value = values[0]
    if value.get('error') != 'kernel 已停止或正在重启；尚未确认本次执行结果':
        raise ValidationError('no_kernel_lifecycle_broadcast_observed')
    if not any(marker in output.get('text', '').splitlines() for output in value.get('outputs', [])):
        raise ValidationError('kernel_death_partial_output_missing')
    pending = current().get('running') or current().get('execution_pending')
    if not pending or pending != value.get('msg_id'):
        raise ValidationError('kernel_death_pending_not_persisted')
    report['checks'].append({'name': 'real_kernel_death_broadcast_is_unknown_with_partial_output', 'passed': True})
    blocked = client.call(bound + ['exec', '-s', name, '--json', '-f', str(script)])
    save_result(directory / 'blocked-exec.json', blocked)
    if (blocked.returncode == 0 or '上次执行尚未确认结束' not in blocked.stderr.decode()
            or pending not in blocked.stderr.decode()):
        raise ValidationError('kernel_death_pending_did_not_block_replay')
    report['checks'].append({'name': 'durable_pending_blocks_same_script_replay', 'passed': True})
    target = directory / 'remote-counter.txt'
    downloaded = client.call(bound + ['download', '-s', name, remote, str(target)])
    save_result(directory / 'download.json', downloaded)
    if downloaded.returncode or target.read_text() != 'once\n':
        raise ValidationError('kernel_death_side_effect_not_exactly_once')
    report['checks'].append({'name': 'independent_contents_confirms_single_side_effect', 'passed': True})
    restarted = client.call(bound + ['restart-kernel', '-s', name])
    save_result(directory / 'restart.json', restarted)
    if restarted.returncode or current().get('running') or current().get('execution_pending'):
        raise ValidationError('kernel_death_explicit_restart_failed')
    recovered = client.call(bound + ['exec', '-s', name, '--json'], data=(
        'from pathlib import Path\nassert Path(' + repr('/' + remote)
        + ').read_text()=="once\\n"\nprint("recovered")\n').encode())
    save_result(directory / 'recovered-exec.json', recovered)
    values = json.loads(recovered.stdout) if recovered.returncode == 0 else []
    if not values or any(value.get('status') != 'succeeded' for value in values):
        raise ValidationError('kernel_death_execution_after_restart_failed')
    report['checks'].append({'name': 'explicit_restart_recovers_without_replaying_crashing_script', 'passed': True})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/colab'))
    parser.add_argument('--config-dir', type=Path, default=Path.home() / '.config/colab-cli')
    parser.add_argument('--account')
    parser.add_argument('--token-storage', choices=('file', 'auto', 'keyring'), default='file')
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--cleanup-only', action='store_true')
    parser.add_argument('--terminal-keyring', action='store_true', help='inherit the controlling terminal for native keychain calls; 120 second per-call keychain wait')
    parser.add_argument('--fault', choices=('transport', 'kernel-death'), default='transport')
    args = parser.parse_args()
    if os.name != 'posix' or (args.execute and args.cleanup_only):
        parser.error('Unix required; execute and cleanup-only conflict')
    if args.terminal_keyring and not os.isatty(0):
        parser.error('terminal-keyring requires a controlling terminal')
    root, binary, directory = args.config_dir.resolve(), args.binary.resolve(), args.state_dir.resolve()
    if directory.is_relative_to(root):
        parser.error('state-dir must be outside configuration')
    env = {k: v for k, v in os.environ.items() if not k.startswith('COLAB_')}
    if args.cleanup_only:
        if directory.stat().st_mode & 0o077:
            parser.error('private state-dir required')
        ledger = json.loads((directory / 'ledger.json').read_text())
        if ledger['config_dir'] != str(root):
            parser.error('ledger configuration differs')
    else:
        directory.mkdir(mode=0o700)
        prefix = [str(binary), '--config-dir', str(root), '--token-storage', args.token_storage]
        if args.account:
            prefix += ['--account', args.account]
        selected = subprocess.run(prefix + ['account', 'current', '--json'], capture_output=True, env=env, timeout=20)
        if selected.returncode:
            raise ValidationError('offline_account_selection_failed')
        value = json.loads(selected.stdout)
        ledger = {'config_dir': str(root), 'account': value['account'], 'provider': value['auth']['provider'],
                  'token_storage': args.token_storage, 'session': 'cut-validation-' + uuid.uuid4().hex[:16],
                  'baseline_endpoints': [], 'allocation_requested': False, 'endpoint': None}
    client_class = TerminalClient if args.terminal_keyring else Client
    client = client_class(binary, root, ledger['account'], ledger['provider'], ledger['token_storage'], env)
    report = {'schema_version': 1, 'passed': False, 'checks': [], 'created_sessions': 0,
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'cleanup': 'not_allocated',
              'terminal_keyring': args.terminal_keyring, 'validator_source_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'fault': args.fault, 'execute_requested': args.execute, 'cleanup_only': args.cleanup_only}
    save = lambda: private_json(directory / 'ledger.json', ledger)
    journal = root / 'accounts' / ledger['account'] / 'control_operation.json'
    save()
    try:
        if not args.cleanup_only:
            if journal.exists():
                raise ValidationError('existing_control_operation_requires_recovery')
            ledger['baseline_endpoints'] = sorted(client.inventory())
            report['credentials_ready'] = True
            report['baseline_assignment_count'] = len(ledger['baseline_endpoints'])
            save()
            if args.execute:
                ledger['allocation_requested'] = True
                save()
                allocated = client.call(['new', '-s', ledger['session'], '--json'], timeout=90)
                save_result(directory / 'allocation.json', allocated)
                if allocated.returncode:
                    raise ValidationError('allocation_command_failed')
                capture_allocation(json.loads(allocated.stdout), ledger)
                report['created_sessions'] = 1
                save()
                if not ledger['is_cpu']:
                    raise ValidationError('allocation_hardware_mismatch')
                if args.fault == 'kernel-death':
                    validate_kernel_death(client, root, ledger, directory, report)
                else:
                    validate(client, root, ledger, directory, report)
            else:
                report['preflight_only'] = True
        report['passed'] = True
    except (ValidationError, OSError, ValueError, subprocess.SubprocessError) as error:
        report['error_class'] = str(error) if isinstance(error, ValidationError) else type(error).__name__
    finally:
        if ledger['allocation_requested'] and not ledger.get('endpoint') and journal.is_file():
            try:
                pending = json.loads(journal.read_text())
            except (OSError, ValueError):
                pending = {}
            if pending.get('operation') == 'allocate' and pending.get('name') == ledger['session']:
                try:
                    allocated = client.call(['new', '-s', ledger['session'], '--json'], timeout=90)
                    if allocated.returncode == 0:
                        capture_allocation(json.loads(allocated.stdout), ledger)
                        report['created_sessions'] = 1
                except (ValidationError, OSError, ValueError, subprocess.SubprocessError):
                    pass
        try:
            report['cleanup'] = cleanup(client, ledger)
        except (ValidationError, OSError, ValueError, subprocess.SubprocessError):
            report['cleanup'] = 'verification_failed'
        report['other_new_assignment_count'] = len(ledger.get('other_new_endpoints', []))
        if ledger['allocation_requested'] and report['cleanup'] != 'confirmed_released':
            report['passed'] = False
        save()
        private_json(directory / 'report.json', report)
        print(json.dumps(report))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
