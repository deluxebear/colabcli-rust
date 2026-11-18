#!/usr/bin/env python3
"""Opt-in real CPU/SSH validation; private ledger, endpoint-bound cleanup, no Drive."""
import argparse
import base64
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import shutil
import shlex
import subprocess
import sys
import time
import uuid


class ValidationError(RuntimeError):
    pass


def private_json(path, value):
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, 'w') as output:
            json.dump(value, output, indent=2)
            output.write('\n')
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


class Client:
    def __init__(self, binary, root, account, provider, mode, env):
        self.prefix = [str(binary), '--config-dir', str(root), '--account', account, '--auth', provider,
                       '--token-storage', mode, '--request-timeout', '20', '--assign-retries', '0']
        self.env, self.account = env, account

    def call(self, args, data=None, timeout=60):
        command = self.prefix + args
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   env=self.env, start_new_session=True)
        try:
            output, error = process.communicate(data, timeout=timeout)
            return subprocess.CompletedProcess(command, process.returncode, output, error)
        except subprocess.TimeoutExpired:
            terminate_group(process)
            raise

    def inventory(self):
        response = self.call(['sessions', '--json'])
        if response.returncode:
            raise ValidationError('inventory_command_failed')
        value = json.loads(response.stdout)
        # Merged JSON commands can exit 0 while a per-account error is present.
        if not isinstance(value, dict) or value.get('error') or value.get('account') != self.account:
            raise ValidationError('inventory_credentials_or_account_error')
        rows = value.get('assignments')
        if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
            raise ValidationError('inventory_missing_assignments')
        endpoints = [row.get('endpoint') for row in rows]
        if any(not isinstance(endpoint, str) or not endpoint for endpoint in endpoints) or len(set(endpoints)) != len(endpoints):
            raise ValidationError('inventory_invalid_assignments')
        return set(endpoints)


def capture_allocation(value, ledger):
    endpoint = value.get('endpoint') if isinstance(value, dict) else None
    if not isinstance(endpoint, str) or not endpoint or endpoint in ledger['baseline_endpoints']:
        raise ValidationError('allocation_ownership_not_proven')
    if value.get('name') != ledger['session']:
        raise ValidationError('allocation_identity_mismatch')
    ledger['endpoint'] = endpoint
    ledger['is_cpu'] = value.get('accelerator') == 'NONE'


def cleanup(client, ledger):
    endpoint = ledger.get('endpoint')
    if not endpoint:
        return 'allocation_unknown' if ledger.get('allocation_requested') else 'not_allocated'
    if endpoint in ledger['baseline_endpoints']:
        return 'ownership_refused'
    response = client.call(['stop', '-s', ledger['session'], '--colab-expected-endpoint', endpoint])
    # The server's absence is authoritative even if a concurrent stop removed
    # local state first. A failed inventory never counts as a released VM.
    remaining = client.inventory()
    ledger['other_new_endpoints'] = sorted(remaining - set(ledger['baseline_endpoints']) - {endpoint})
    if endpoint not in remaining:
        return 'confirmed_released'
    return 'pending' if response.returncode else 'release_not_confirmed'


def terminate_group(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate(timeout=5)


def probe_banner(client, name, identity, endpoint):
    process = subprocess.Popen(client.prefix + ['ssh', '--proxy-mode', '-s', name, '-i', str(identity), '--colab-expected-endpoint', endpoint],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=client.env, start_new_session=True)
    try:
        process.stdin.write(b'SSH-2.0-colabcli_validation\r\n')
        process.stdin.flush()
        reader = selectors.DefaultSelector()
        try:
            reader.register(process.stdout, selectors.EVENT_READ)
            data = b''
            deadline = time.monotonic() + 25
            while b'\n' not in data and len(data) < 4096:
                wait = deadline - time.monotonic()
                if wait <= 0 or not reader.select(wait):
                    raise ValidationError('ssh_banner_timeout')
                chunk = os.read(process.stdout.fileno(), 4096)
                if not chunk:
                    raise ValidationError('ssh_banner_eof')
                data += chunk
            if not data.split(b'\n', 1)[0].startswith(b'SSH-2.0-'):
                raise ValidationError('ssh_banner_invalid')
        finally:
            reader.close()
        process.stdin.close()
        process.stdin = None
        _, _ = process.communicate(timeout=15)
        if process.returncode:
            raise ValidationError('ssh_proxy_close_failed')
    finally:
        if process.poll() is None:
            terminate_group(process)
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                stream.close()


def probe_interactive(client, name, identity, endpoint, timeout=60, diagnostic=None):
    """Use an actual local PTY and shell; echoed input cannot satisfy the probe."""
    import pty
    master, slave = pty.openpty()
    process = None
    reader = selectors.DefaultSelector()
    marker = 'COLAB_PTY_' + uuid.uuid4().hex
    code = ('import json,os,sys;print(' + repr(marker) + '+json.dumps({'
            '"tty":[os.isatty(i) for i in range(3)],"cwd":os.getcwd(),'
            '"ssh_tty":bool(os.environ.get("SSH_TTY"))}))')
    command = ('python3 -c ' + shlex.quote(code) + '\nexit 0\n').encode()
    output = b''
    try:
        process = subprocess.Popen(client.prefix + ['ssh', '-s', name, '-i', str(identity),
                                   '--colab-expected-endpoint', endpoint],
                                   stdin=slave, stdout=slave, stderr=slave,
                                   env=client.env, start_new_session=True)
        os.close(slave)
        slave = None
        os.write(master, command)
        reader.register(master, selectors.EVENT_READ)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ValidationError('interactive_pty_timeout')
            if not reader.select(min(remaining, .1)):
                if process.poll() is not None:
                    break
                continue
            try:
                chunk = os.read(master, 8192)
            except OSError as error:
                if error.errno == errno.EIO:
                    break  # Linux PTY EOF; macOS returns empty bytes.
                raise
            if not chunk:
                break
            output += chunk
            if len(output) > 1024 * 1024:
                raise ValidationError('interactive_pty_output_limit')
        process.wait(timeout=max(.1, deadline - time.monotonic()))
        lines = [line for line in output.splitlines() if line.startswith(marker.encode())]
        if process.returncode or len(lines) != 1:
            raise ValidationError('interactive_shell_result_missing')
        value = json.loads(lines[0][len(marker):])
        if value != {'tty': [True, True, True], 'cwd': '/content', 'ssh_tty': True}:
            raise ValidationError('interactive_remote_pty_mismatch')
    finally:
        if process is not None and process.poll() is None:
            terminate_group(process)
        if diagnostic is not None:
            private_json(diagnostic, {'stdout_base64': base64.b64encode(output).decode(),
                                     'exit_code': process.returncode if process is not None else None})
        reader.close()
        os.close(master)
        if slave is not None:
            os.close(slave)


def probe_command_signal(client, name, identity, endpoint, timeout=60):
    """Send TERM only after remote code, rather than just the SSH banner, ran."""
    marker = ('COLAB_SIGNAL_' + uuid.uuid4().hex).encode()
    code = 'import time;print(' + repr(marker.decode()) + ',flush=True);time.sleep(180)'
    process = subprocess.Popen(client.prefix + ['ssh', '-s', name, '-i', str(identity),
                               '--colab-expected-endpoint', endpoint, '--', 'python3', '-c', code],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                               env=client.env, start_new_session=True)
    reader = selectors.DefaultSelector()
    try:
        reader.register(process.stdout, selectors.EVENT_READ)
        output = b''
        deadline = time.monotonic() + timeout
        while marker not in output.splitlines():
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not reader.select(remaining):
                raise ValidationError('signal_remote_marker_timeout')
            chunk = os.read(process.stdout.fileno(), 8192)
            if not chunk:
                raise ValidationError('signal_remote_marker_eof')
            output += chunk
            if len(output) > 1024 * 1024:
                raise ValidationError('signal_probe_output_limit')
        process.send_signal(signal.SIGTERM)
        process.communicate(timeout=15)
        if process.returncode != 143:
            raise ValidationError('ssh_sigterm_exit_mismatch')
    finally:
        if process.poll() is None:
            terminate_group(process)
        reader.close()
        for stream in (process.stdin, process.stdout):
            stream.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/colab'))
    parser.add_argument('--config-dir', type=Path, default=Path.home() / '.config/colab-cli')
    parser.add_argument('--account')
    parser.add_argument('--token-storage', choices=('file', 'auto', 'keyring'), default='file')
    parser.add_argument('--state-dir', type=Path, required=True, help='new private directory outside the CLI configuration')
    parser.add_argument('--execute', action='store_true', help='allocate exactly one owned CPU VM and test SSH')
    parser.add_argument('--extended', action='store_true', help='also verify real interactive PTY and command SIGTERM (requires --execute)')
    parser.add_argument('--cleanup-only', action='store_true', help='recover only the ledger VM; never repeat SSH commands')
    args = parser.parse_args()
    if os.name != 'posix':
        parser.error('live SSH validation currently requires Unix process groups')
    if args.execute and args.cleanup_only:
        parser.error('--execute and --cleanup-only conflict')
    if args.extended and not args.execute:
        parser.error('--extended requires --execute')
    root, binary, state = args.config_dir.resolve(), args.binary.resolve(), args.state_dir.resolve()
    if state.is_relative_to(root):
        parser.error('--state-dir must be outside the CLI config directory')
    env = {key: value for key, value in os.environ.items() if not key.startswith('COLAB_')}
    for key in ('SSH_AUTH_SOCK', 'SSH_ASKPASS', 'DISPLAY'):
        env.pop(key, None)
    if args.cleanup_only:
        if not state.is_dir() or state.stat().st_mode & 0o077:
            parser.error('existing private state directory required')
        ledger = json.loads((state / 'ledger.json').read_text())
        if ledger.get('schema_version') != 1 or ledger['config_dir'] != str(root):
            parser.error('ledger configuration differs')
        account, provider, mode = ledger['account'], ledger['provider'], ledger['token_storage']
    else:
        state.mkdir(mode=0o700)
        prefix = [str(binary), '--config-dir', str(root), '--token-storage', args.token_storage]
        if args.account:
            prefix += ['--account', args.account]
        response = subprocess.run(prefix + ['account', 'current', '--json'], capture_output=True, env=env, timeout=20)
        if response.returncode:
            raise ValidationError('offline_account_selection_failed')
        selected = json.loads(response.stdout)
        account, provider, mode = selected['account'], selected['auth']['provider'], args.token_storage
        ledger = {'schema_version': 1, 'config_dir': str(root), 'account': account, 'provider': provider,
                  'token_storage': mode, 'session': 'ssh-validation-' + uuid.uuid4().hex[:16],
                  'baseline_endpoints': [], 'allocation_requested': False, 'endpoint': None}
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._@-]{0,63}', account):
        raise ValidationError('invalid_profile')
    client = Client(binary, root, account, provider, mode, env)
    report = {'schema_version': 1, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'passed': False, 'checks': [], 'execute_requested': args.execute,
              'cleanup_only': args.cleanup_only, 'cleanup': 'not_allocated', 'created_sessions': 0}
    save = lambda: private_json(state / 'ledger.json', ledger)
    save()
    journal = root / 'accounts' / account / 'control_operation.json'
    keys = state / 'keys'
    try:
        if not args.cleanup_only:
            if journal.exists():
                raise ValidationError('existing_control_operation_requires_recovery')
            ledger['baseline_endpoints'] = sorted(client.inventory())
            report['baseline_assignment_count'] = len(ledger['baseline_endpoints'])
            save()
            report['credentials_ready'] = True
            if not args.execute:
                report['preflight_only'] = True
                report['passed'] = True
                return 0
            keys.mkdir(mode=0o700)
            identity = keys / 'id_ed25519'
            subprocess.run(['/usr/bin/ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-C', 'colabcli-synthetic-live-test', '-f', str(identity)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True, timeout=20)
            wrapper = keys / 'ssh'
            wrapper.write_text('#!/bin/sh\nexec /usr/bin/ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=20 -o ConnectionAttempts=1 "$@"\n')
            wrapper.chmod(0o700)
            client.env = dict(env, PATH=str(keys) + os.pathsep + env.get('PATH', os.defpath))
            ledger['allocation_requested'] = True
            save()
            allocation = client.call(['new', '-s', ledger['session'], '--json'], timeout=90)
            if allocation.returncode:
                raise ValidationError('allocation_command_failed')
            capture_allocation(json.loads(allocation.stdout), ledger)
            report['created_sessions'] = 1
            save()
            if not ledger['is_cpu']:
                raise ValidationError('allocation_hardware_mismatch')
            probe_banner(client, ledger['session'], identity, ledger['endpoint'])
            report['checks'].append({'name': 'raw_proxy_banner_and_eof', 'passed': True})
            marker = uuid.uuid4().hex
            cases = [
                ('identity_and_workdir', ['python3', '-c', 'import os,json;print(json.dumps({"marker":' + repr(marker) + ',"uid":os.getuid(),"cwd":os.getcwd()}))'], b'', 0,
                 lambda output: json.loads(output) == {'marker': marker, 'uid': 0, 'cwd': '/content'}),
                ('quoted_argv', ['python3', '-c', 'import sys,json;print(json.dumps(sys.argv[1:],ensure_ascii=False))', '--flag', 'space arg', "quote'\"\\雪", '--env'], b'', 0,
                 lambda output: json.loads(output) == ['--flag', 'space arg', "quote'\"\\雪", '--env']),
                ('binary_stdio', ['python3', '-c', 'import sys;sys.stdout.buffer.write(sys.stdin.buffer.read())'], bytes(range(256)) * 4, 0,
                 lambda output: output == bytes(range(256)) * 4),
                ('remote_exit_code', ['sh', '-c', 'exit 23'], b'', 23, lambda output: output == b''),
            ]
            for name, command, data, code, check in cases:
                result = client.call(['ssh', '-s', ledger['session'], '-i', str(identity), '--colab-expected-endpoint', ledger['endpoint'], '--'] + command, data=data, timeout=60)
                passed = result.returncode == code and check(result.stdout)
                report['checks'].append({'name': name, 'exit_code': result.returncode, 'passed': passed})
                if not passed:
                    # Server diagnostics may contain sensitive routing details.
                    # Keep exact bytes private; the public report only names the check.
                    private_json(state / 'failed-check.json', {
                        'name': name, 'exit_code': result.returncode,
                        'stdout_base64': base64.b64encode(result.stdout).decode(),
                        'stderr_base64': base64.b64encode(result.stderr).decode(),
                    })
                    raise ValidationError('ssh_check_failed:' + name)
            if args.extended:
                probe_interactive(client, ledger['session'], identity, ledger['endpoint'],
                                  diagnostic=state / 'interactive-check.json')
                report['checks'].append({'name': 'interactive_shell_remote_pty', 'passed': True})
                probe_command_signal(client, ledger['session'], identity, ledger['endpoint'])
                report['checks'].append({'name': 'command_sigterm_after_remote_marker', 'exit_code': 143, 'passed': True})
        report['passed'] = True
    except (ValidationError, OSError, ValueError, subprocess.SubprocessError) as error:
        report['error_class'] = str(error) if isinstance(error, ValidationError) else type(error).__name__
    finally:
        if ledger['allocation_requested'] and not ledger.get('endpoint') and journal.is_file():
            try:
                pending = json.loads(journal.read_text())
            except (OSError, ValueError):
                pending = {}
            if isinstance(pending, dict) and pending.get('operation') == 'allocate' and pending.get('name') == ledger['session']:
                # The identical name/options reuse the durable notebook identity.
                # Reconcile once; never repeat SSH or adopt an unrelated session.
                try:
                    allocation = client.call(['new', '-s', ledger['session'], '--json'], timeout=90)
                    if allocation.returncode == 0:
                        capture_allocation(json.loads(allocation.stdout), ledger)
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
        if keys.is_dir():
            shutil.rmtree(keys)
        save()
        private_json(state / 'report.json', report)
        print(json.dumps(report))
        if ledger['allocation_requested'] and report['cleanup'] != 'confirmed_released':
            print('Cleanup pending; account=' + account + ' endpoint=' + str(ledger.get('endpoint')) + '; private ledger=' + str(state / 'ledger.json'), file=sys.stderr)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
