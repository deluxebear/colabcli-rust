#!/usr/bin/env python3
"""Opt-in bounded DriveFS validation. Default: local bundle inspection, no CLI/network."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys

from prepare_live_drive_validation import profile
from drive_api_validation import DriveObserver, ObservationError
from drive_validation_payload import OWNER, DATA, PAYLOAD, owner_value
from validate_live_ssh import Client, ValidationError, capture_allocation, private_json


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_json(path, private=False):
    if path.resolve() != path:
        raise ValidationError('symlinked_input_refused')
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or (private and info.st_mode & 0o077):
            raise ValidationError('unsafe_input_permissions_or_type')
        value = stream.read(4 * 1024 * 1024 + 1)
        if len(value) > 4 * 1024 * 1024:
            raise ValidationError('input_too_large')
        return json.loads(value)


def credential_identity(value):
    if not isinstance(value, dict) or not all(isinstance(value.get(key), str) and value[key] for key in ('client_id', 'user_id', 'refresh_token')):
        raise ValidationError('credential_identity_missing')
    return hashlib.sha256(json.dumps([value['client_id'], value['user_id']], ensure_ascii=True).encode()).hexdigest()


def load_bundle(directory):
    if not directory.is_dir() or directory.stat().st_mode & 0o077:
        raise ValidationError('private_state_directory_required')
    plan = read_json(directory / 'plan.json', True)
    prepared = read_json(directory / 'preparation.json', True)
    if not isinstance(plan, dict) or not isinstance(prepared, dict):
        raise ValidationError('prepared_bundle_invalid')
    nonce = plan.get('nonce')
    if (type(plan.get('schema_version')) is not int or plan['schema_version'] != 1
            or not isinstance(nonce, str) or not re.fullmatch('[a-f0-9]{32}', nonce)
            or plan.get('session') != 'drive-validation-' + nonce[:16]
            or plan.get('mountpoint') != '/content/colabcli-drive-' + nonce
            or plan.get('folder') != 'colabcli-validation-' + nonce
            or plan.get('endpoint') is not None or plan.get('execution_authorized') is not False
            or plan.get('executed') is not False or plan.get('mode') != 'shim'
            or plan.get('auth_provider') not in ('oauth2', 'adc')
            or plan.get('token_storage') not in ('file', 'auto', 'keyring')):
        raise ValidationError('prepared_plan_invalid')
    for name in ('compute_account', 'drive_account'):
        profile(plan[name])
    root = Path(plan['config_dir'])
    if not root.is_absolute() or root.resolve() != root or directory.is_relative_to(root):
        raise ValidationError('invalid_configuration_root')
    binary = Path(plan['binary'])
    if not binary.is_absolute() or binary.resolve() != binary or digest(binary) != plan['binary_sha256']:
        raise ValidationError('prepared_binary_changed')
    template = Path(__file__).with_name('drive_validation_payload.py').read_text()
    constants = {'nonce': nonce, 'mountpoint': plan['mountpoint'], 'folder': plan['folder']}
    for phase in ('create', 'readonly', 'cleanup'):
        path = directory / (phase + '.py')
        if path.resolve() != path or path.stat().st_mode & 0o077:
            raise ValidationError('unsafe_payload_file')
        expected = 'PLAN = ' + repr(constants) + '\nPHASE = ' + repr(phase) + '\n' + template
        if path.read_text() != expected or digest(path) != prepared['payload_sha256'][path.name]:
            raise ValidationError('prepared_payload_changed')
    registry = read_json(root / 'accounts.json')
    if any(name not in registry['accounts'] for name in (plan['compute_account'], plan['drive_account'])):
        raise ValidationError('selected_profile_missing')
    value = read_json(root / 'accounts' / plan['drive_account'] / 'dfs_credentials.json')
    if (not isinstance(value, dict) or credential_identity(value) != plan.get('drive_identity_sha256')
            or not isinstance(value.get('scopes'), list)
            or 'https://www.googleapis.com/auth/drive' not in value['scopes']):
        raise ValidationError('prepared_drive_identity_changed')
    return plan


class Executor:
    def __init__(self, directory, plan, ledger, client, observer_factory=DriveObserver):
        self.directory, self.plan, self.ledger, self.client = directory, plan, ledger, client
        self.root = Path(plan['config_dir'])
        self.observer_factory = observer_factory

    def save(self):
        private_json(self.directory / 'execution.json', self.ledger)

    def account_path(self, name):
        return self.root / 'accounts' / name

    def runtime_state(self):
        states = read_json(self.account_path(self.plan['compute_account']) / 'sessions.json')
        state = states.get(self.plan['session'])
        if not isinstance(state, dict) or state.get('endpoint') != self.ledger.get('endpoint'):
            raise ValidationError('owned_session_missing_or_replaced')
        if ('drive_mounts' in state and not isinstance(state['drive_mounts'], dict)) or any(
                key in state and not isinstance(state[key], dict) for key in ('drive_plan', 'drive_probe', 'automation_pending')):
            raise ValidationError('runtime_recovery_state_malformed')
        return state

    def credential(self):
        value = read_json(self.account_path(self.plan['drive_account']) / 'dfs_credentials.json')
        if credential_identity(value) != self.plan['drive_identity_sha256']:
            raise ValidationError('drive_identity_changed')
        return value

    def call(self, args, runtime=False, timeout=120, data=None):
        if digest(Path(self.plan['binary'])) != self.plan['binary_sha256']:
            raise ValidationError('binary_changed_during_execution')
        if digest(self.directory / 'plan.json') != self.ledger['plan_sha256']:
            raise ValidationError('plan_changed_during_execution')
        if runtime:
            self.runtime_state()
            args = ['--colab-runtime-endpoint', self.ledger['endpoint']] + args
        return self.client.call(args, timeout=timeout, data=data)

    def operation(self, name, kind, args, validate, runtime=False, data=None):
        if self.ledger.get('inflight'):
            raise ValidationError('unknown_operation_requires_inspection')
        before = self.runtime_state() if runtime else {}
        self.ledger['inflight'] = {'name': name, 'kind': kind,
                                   'kernel_generation': before.get('kernel_generation')}
        self.save()
        result = self.call(args, runtime=runtime, data=data)
        try:
            value = json.loads(result.stdout)
        except ValueError:
            raise ValidationError('operation_reply_not_json') from None
        terminal_execution = kind == 'exec' and isinstance(value, list) and value and all(
            isinstance(row, dict) and row.get('status') in ('succeeded', 'failed') for row in value)
        if result.returncode:
            if terminal_execution:
                # Known terminal exception may leave partial files. The bounded
                # cleanup probe will verify every object before deleting it.
                self.ledger['inflight'] = None
                self.save()
            raise ValidationError('operation_failed:' + name)
        try:
            receipt = validate(value)
        except ValidationError:
            if terminal_execution:
                self.ledger['inflight'] = None
                self.save()
            raise
        if runtime:
            self.runtime_state()
        self.ledger['completed'].append({'name': name, 'receipt': receipt})
        self.ledger['inflight'] = None
        self.save()
        return receipt

    def assert_unmounted_locally(self):
        state = self.runtime_state()
        if state.get('drive_mounts') or any(key in state for key in ('drive_plan', 'automation_pending', 'drive_probe')):
            raise ValidationError('local_drive_recovery_or_mount_still_active')
        registry = read_json(self.root / 'accounts.json')
        for account in registry['accounts']:
            profile(account)
            for path in (self.account_path(account) / 'managed').glob('drivefs-*.json'):
                lease = read_json(path)
                if lease.get('compute_account') == self.plan['compute_account'] and lease.get('endpoint') == self.ledger['endpoint']:
                    raise ValidationError('credential_lease_still_active')

    def mount(self, readonly):
        def validate(value):
            if (not isinstance(value, dict) or value.get('ok') is not True or value.get('mounted') is not True
                    or value.get('stage') != 'mounted' or value.get('mode') != 'shim'
                    or value.get('readonly') is not readonly or value.get('managed') is not True
                    or value.get('identity') != self.plan['drive_identity_sha256']
                    or value.get('mountpoint') != self.plan['mountpoint']
                    or value.get('drive_alive') is not True or value.get('helper_alive') is not True):
                raise ValidationError('mount_reply_not_owned_healthy_identity')
            owner = value.get('owner_operation_id')
            lease = self.runtime_state().get('drive_mounts', {}).get(self.plan['mountpoint'], {})
            if (not isinstance(owner, str) or not re.fullmatch('[a-f0-9]{32}', owner)
                    or lease.get('request', {}).get('operation_id') != owner
                    or lease.get('identity') != self.plan['drive_identity_sha256']
                    or any(lease.get(k) != self.plan[p] for k, p in (
                        ('compute_account', 'compute_account'), ('drive_account', 'drive_account'), ('session', 'session')))
                    or lease.get('endpoint') != self.ledger['endpoint']):
                raise ValidationError('mount_lease_mismatch')
            self.ledger['mounted'] = True
            self.ledger['readonly'] = readonly
            self.ledger['mount_owner'] = owner
            return {'readonly': readonly, 'owner': owner}
        args = ['drive', 'mount', '-s', self.plan['session'], '--path', self.plan['mountpoint'],
                '--drive-account', self.plan['drive_account'], '--mode', 'shim', '--port', '0', '--json']
        if readonly:
            args.append('--readonly')
        self.operation('mount_' + str(len(self.ledger['completed'])), 'mount', args, validate, True)

    def unmount(self):
        def validate(value):
            if (not isinstance(value, dict) or value.get('ok') is not True or value.get('mounted') is not False
                    or value.get('mountpoint') != self.plan['mountpoint']
                    or value.get('stage') not in ('finished', 'unmanaged')):
                raise ValidationError('unmount_not_confirmed')
            self.assert_unmounted_locally()
            self.credential()  # Rotation can change token, never the intended identity.
            self.ledger['mounted'] = False
            return {'unmounted_and_credential_lease_released': True}
        self.operation('umount_' + str(len(self.ledger['completed'])), 'umount',
                       ['drive', 'umount', '-s', self.plan['session'], '--path', self.plan['mountpoint'],
                        '--purge-state', '--json'], validate, True)

    def probe(self, phase):
        source = self.directory / (phase + '.py')
        descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077:
                raise ValidationError('unsafe_payload_file')
            code = stream.read(1024 * 1024 + 1)
        constants = {k: self.plan[k] for k in ('nonce', 'mountpoint', 'folder')}
        expected = ('PLAN = ' + repr(constants) + '\nPHASE = ' + repr(phase) + '\n'
                    + Path(__file__).with_name('drive_validation_payload.py').read_text()).encode()
        if code != expected:
            raise ValidationError('payload_changed_before_dispatch')
        if phase == 'create':
            self.ledger['cloud_create_attempted'] = True
            self.save()
        def validate(rows):
            if not isinstance(rows, list) or not rows or any(not isinstance(row, dict) or row.get('status') != 'succeeded' for row in rows):
                raise ValidationError('probe_execution_not_successful')
            results = []
            for row in rows:
                for output in row.get('outputs', []):
                    if output.get('output_type') == 'stream' and output.get('name') == 'stdout':
                        for line in output.get('text', '').splitlines():
                            try:
                                results.append(json.loads(line))
                            except ValueError:
                                raise ValidationError('unexpected_probe_stdout') from None
            expected = {'create': ('created_and_read_back',), 'readonly': ('write_denied',),
                        'cleanup': ('removed', 'already_absent')}[phase]
            if (len(results) != 1 or not isinstance(results[0], dict)
                    or results[0].get('phase') != phase or results[0].get('status') not in expected
                    or (phase == 'readonly' and results[0].get('original_bytes_unchanged') is not True)):
                raise ValidationError('probe_result_mismatch')
            if phase == 'cleanup':
                self.ledger['cloud_cleanup_confirmed'] = True
            if phase == 'readonly':
                self.ledger['readonly_verified'] = True
            return {'phase': phase, 'status': results[0]['status']}
        self.operation('probe_' + phase, 'exec', ['exec', '-s', self.plan['session'], '--json'],
                       validate, True, data=code)

    def allocate(self):
        def validate(value):
            capture_allocation(value, self.ledger)
            self.save()  # Save ownership even if the server unexpectedly chose a GPU.
            if not self.ledger['is_cpu']:
                self.ledger['inflight'] = None
                self.save()
                raise ValidationError('allocation_hardware_mismatch')
            return {'cpu': True}
        self.ledger['allocation_requested'] = True
        self.save()
        self.operation('allocate', 'allocate', ['new', '-s', self.plan['session'], '--json'], validate)

    def run(self):
        root = self.account_path(self.plan['compute_account'])
        drive = self.account_path(self.plan['drive_account'])
        if any(path.exists() for path in (self.root / 'account-transaction.json', root / 'control_operation.json',
                                         drive / 'control_operation.json', drive / 'dfs_pending.json',
                                         drive / 'dfs_refresh_pending.json')):
            raise ValidationError('existing_operation_requires_recovery')
        if list((drive / 'managed').glob('drivefs-*.json')):
            raise ValidationError('existing_drive_lease_requires_separate_validation')
        states = read_json(root / 'sessions.json') if (root / 'sessions.json').exists() else {}
        if self.plan['session'] in states:
            raise ValidationError('prepared_session_name_already_exists')
        self.ledger['baseline_endpoints'] = sorted(self.client.inventory())
        self.save()
        before = self.credential()
        response = self.call(['drive', 'token', '--drive-account', self.plan['drive_account'], '--whoami', '--json'])
        value = json.loads(response.stdout)  # Never persist either stream from token command.
        if (response.returncode or not isinstance(value, dict) or value.get('account') != self.plan['drive_account']
                or value.get('user_id') != before['user_id'] or not isinstance(value.get('access_token'), str)
                or not value['access_token'] or not isinstance(value.get('scope'), str)
                or 'https://www.googleapis.com/auth/drive' not in value.get('scope', '').split()):
            raise ValidationError('fresh_drive_identity_not_confirmed')
        self.credential()
        observer = self.observer_factory(value['access_token'])
        observer.verify_identity()
        del before, value, response
        self.ledger['same_token_drive_ownership_verified'] = True
        self.ledger['credential_userinfo_verified'] = True
        self.save()
        self.allocate()
        self.mount(False)
        self.probe('create')
        self.unmount()
        # Observe after unmount, independent of the FUSE page cache. Reads only;
        # incomplete server synchronization fails closed and enters cleanup.
        observed = observer.inspect(self.plan['folder'], {
            OWNER: json.dumps(owner_value(self.plan['nonce'], self.plan['folder']), sort_keys=True).encode(),
            DATA: PAYLOAD})
        if observed.get('server_bytes_verified') is not True:
            raise ValidationError('drive_api_server_bytes_unconfirmed')
        self.ledger['server_bytes_verified'] = True
        self.save()
        self.mount(True)
        self.probe('readonly')
        self.unmount()
        self.mount(False)
        self.probe('cleanup')
        self.unmount()
        removed = observer.verify_removed()
        if removed.get('server_cleanup_verified') is not True:
            raise ValidationError('drive_api_server_cleanup_unconfirmed')
        self.ledger['server_cleanup_verified'] = True
        self.ledger['server_cleanup_counts'] = {k: removed[k] for k in ('trashed_objects', 'missing_objects')}
        self.ledger['validation_steps_passed'] = True
        self.save()

    def cleanup(self):
        inflight = self.ledger.get('inflight')
        if not self.ledger.get('endpoint'):
            if self.ledger.get('allocation_requested'):
                journal = self.account_path(self.plan['compute_account']) / 'control_operation.json'
                if journal.exists() and not self.ledger.get('allocation_reconciliation_attempted'):
                    pending = read_json(journal)
                    if pending.get('operation') != 'allocate' or pending.get('name') != self.plan['session']:
                        return 'allocation_unknown'
                    self.ledger['allocation_reconciliation_attempted'] = True
                    self.save()
                    result = self.call(['new', '-s', self.plan['session'], '--json'])
                    if result.returncode:
                        return 'allocation_unknown'
                    capture_allocation(json.loads(result.stdout), self.ledger)
                elif not journal.exists():
                    path = self.account_path(self.plan['compute_account']) / 'sessions.json'
                    row = read_json(path).get(self.plan['session']) if path.exists() else None
                    if not row or row.get('endpoint') not in self.client.inventory():
                        return 'allocation_unknown'
                    capture_allocation(row, self.ledger)
                else:
                    return 'allocation_unknown'
                self.ledger['inflight'] = inflight = None
                self.save()
            else:
                return 'not_allocated'
        if self.ledger.get('endpoint') in self.ledger['baseline_endpoints']:
            raise ValidationError('baseline_endpoint_cleanup_refused')
        if self.ledger.get('released_confirmed') or (inflight and inflight['kind'] == 'stop'):
            remaining = self.client.inventory()
            self.record_other_endpoints(remaining)
            if self.ledger['endpoint'] not in remaining:
                self.ledger['inflight'] = None
                self.ledger['released_confirmed'] = True
                self.save()
                return 'confirmed_released'
            if self.ledger.get('released_confirmed'):
                return 'pending_recovery'
            # Native guarded stop resumes only its captured release transaction.
            self.call(['stop', '-s', self.plan['session'], '--colab-expected-endpoint', self.ledger['endpoint']])
            remaining = self.client.inventory()
            self.record_other_endpoints(remaining)
            if self.ledger['endpoint'] in remaining:
                return 'pending_recovery'
            self.ledger['inflight'] = None
            self.ledger['released_confirmed'] = True
            self.save()
            return 'confirmed_released'
        if inflight:
            # The only automatic recovery is evidence of an explicit successful
            # restart of our probe kernel. Mount/unmount recovery stays in CLI.
            state = self.runtime_state()
            if inflight['kind'] == 'allocate':
                self.assert_unmounted_locally()
            elif inflight['kind'] in ('mount', 'umount'):
                if not self.observe_recovered_drive(state, inflight['kind']):
                    return 'pending_recovery'
            elif (inflight['kind'] != 'exec' or not state.get('kernel_generation')
                    or state.get('kernel_generation') == inflight.get('kernel_generation')
                    or any(state.get(k) for k in ('running', 'execution_pending', 'kernel_restart_pending',
                                                  'kernel_start_pending', 'drive_plan', 'automation_pending', 'drive_probe'))):
                return 'pending_recovery'
            self.ledger['inflight'] = None
            self.save()
        if self.ledger.get('cloud_create_attempted') and not self.ledger.get('cloud_cleanup_confirmed'):
            if self.ledger.get('mounted') and self.ledger.get('readonly'):
                self.unmount()
            if not self.ledger.get('mounted'):
                self.mount(False)
            self.probe('cleanup')
        if self.ledger.get('mounted'):
            self.unmount()
        self.assert_unmounted_locally()
        self.ledger['inflight'] = {'kind': 'stop', 'name': 'stop'}
        self.save()
        self.call(['stop', '-s', self.plan['session'], '--colab-expected-endpoint', self.ledger['endpoint']])
        remaining = self.client.inventory()
        self.record_other_endpoints(remaining)
        if self.ledger['endpoint'] in remaining:
            return 'pending_recovery'
        self.ledger['inflight'] = None
        self.ledger['released_confirmed'] = True
        self.save()
        return 'confirmed_released'

    def record_other_endpoints(self, remaining):
        self.ledger['other_new_assignment_count'] = len(remaining - set(self.ledger['baseline_endpoints']) - {self.ledger['endpoint']})

    def observe_recovered_drive(self, state, kind):
        if any(k in state for k in ('drive_plan', 'automation_pending', 'drive_probe')):
            return False
        response = self.call(['drive', 'status', '-s', self.plan['session'], '--drive-account', self.plan['drive_account'],
                              '--path', self.plan['mountpoint'], '--json'], True)
        value = json.loads(response.stdout)
        if (response.returncode or not isinstance(value, dict) or value.get('ok') is not True
                or value.get('mountpoint') != self.plan['mountpoint']):
            return False
        if value.get('mounted') is False and value.get('stage') in ('finished', 'unmanaged'):
            self.assert_unmounted_locally()
            self.credential()
            self.ledger['mounted'] = False
            return True
        lease = self.runtime_state().get('drive_mounts', {}).get(self.plan['mountpoint'], {})
        owner = lease.get('request', {}).get('operation_id')
        if (value.get('stage') != 'mounted' or value.get('mounted') is not True
                or value.get('drive_alive') is not True or value.get('helper_alive') is not True
                or value.get('mode') != 'shim' or value.get('identity') != self.plan['drive_identity_sha256']
                or not isinstance(owner, str) or not re.fullmatch('[a-f0-9]{32}', owner)
                or value.get('owner_operation_id') != owner
                or lease.get('identity') != self.plan['drive_identity_sha256']
                or lease.get('endpoint') != self.ledger['endpoint']
                or lease.get('compute_account') != self.plan['compute_account']
                or lease.get('drive_account') != self.plan['drive_account']
                or lease.get('session') != self.plan['session']
                or type(value.get('readonly')) is not bool
                or value['readonly'] != lease.get('request', {}).get('readonly')
                or (kind == 'umount' and owner != self.ledger.get('mount_owner'))):
            return False
        self.ledger.update(mounted=True, readonly=value['readonly'], mount_owner=owner)
        return True


def execute(directory, plan, cleanup_only=False, client_factory=Client, observer_factory=DriveObserver):
    path = directory / 'execution.json'
    if cleanup_only:
        ledger = read_json(path, True)
        if (not isinstance(ledger, dict) or type(ledger.get('schema_version')) is not int or ledger['schema_version'] != 1
                or ledger.get('session') != plan['session']
                or ledger.get('plan_sha256') != digest(directory / 'plan.json')
                or ledger.get('cloud_scope_authorized') is not True
                or not isinstance(ledger.get('baseline_endpoints'), list)
                or any(not isinstance(x, str) for x in ledger['baseline_endpoints'])
                or type(ledger.get('allocation_requested')) is not bool or not isinstance(ledger.get('completed'), list)
                or (ledger.get('inflight') is not None and (not isinstance(ledger['inflight'], dict)
                    or ledger['inflight'].get('kind') not in ('allocate', 'exec', 'mount', 'umount', 'stop')))
                or any(k in ledger and type(ledger[k]) is not bool for k in (
                    'released_confirmed', 'cloud_create_attempted', 'cloud_cleanup_confirmed', 'mounted', 'readonly'))
                or (ledger.get('endpoint') is not None and not isinstance(ledger['endpoint'], str))
                or (ledger.get('released_confirmed') and ledger.get('cloud_create_attempted')
                    and not ledger.get('cloud_cleanup_confirmed'))):
            raise ValidationError('execution_ledger_mismatch')
    else:
        if path.exists():
            raise ValidationError('execution_already_started_use_cleanup_only')
        ledger = {'schema_version': 1, 'plan_sha256': digest(directory / 'plan.json'),
                  'cloud_scope_authorized': True, 'session': plan['session'], 'baseline_endpoints': [],
                  'allocation_requested': False, 'endpoint': None, 'completed': [], 'inflight': None}
    env = {k: v for k, v in os.environ.items() if not k.startswith('COLAB_')}
    client = client_factory(Path(plan['binary']), Path(plan['config_dir']), plan['compute_account'],
                            plan['auth_provider'], plan['token_storage'], env)
    engine = Executor(directory, plan, ledger, client, observer_factory)
    report = {'schema_version': 1, 'passed': False, 'cleanup_only': cleanup_only,
              'binary_sha256': plan['binary_sha256'], 'independent_drive_api_identity_verified': False}
    engine.save()
    try:
        if not cleanup_only:
            engine.run()
    except (ValidationError, ObservationError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        report['error_class'] = str(error) if isinstance(error, (ValidationError, ObservationError)) else type(error).__name__
    finally:
        try:
            report['cleanup'] = engine.cleanup()
        except (ValidationError, ObservationError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
            report['cleanup'] = 'pending_recovery'
            report['cleanup_error_class'] = str(error) if isinstance(error, (ValidationError, ObservationError)) else type(error).__name__
        report.update(created_sessions=int(bool(ledger.get('endpoint'))),
                      credential_userinfo_verified=ledger.get('credential_userinfo_verified', False),
                      same_token_drive_ownership_verified=ledger.get('same_token_drive_ownership_verified', False),
                      server_bytes_verified=ledger.get('server_bytes_verified', False),
                      server_cleanup_verified=ledger.get('server_cleanup_verified', False),
                      server_cleanup_counts=ledger.get('server_cleanup_counts'),
                      readonly_verified=ledger.get('readonly_verified', False),
                      cloud_cleanup_confirmed=ledger.get('cloud_cleanup_confirmed', False),
                      other_new_assignment_count=ledger.get('other_new_assignment_count'))
        report['passed'] = (report['cleanup'] in ('confirmed_released', 'not_allocated') if cleanup_only else
                            report['cleanup'] == 'confirmed_released' and ledger.get('validation_steps_passed', False))
        engine.save()
        private_json(directory / 'execution-report.json', report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--allow-cloud-test-directory', help='must exactly match the prepared random folder')
    parser.add_argument('--cleanup-only', action='store_true')
    args = parser.parse_args()
    if args.execute and args.cleanup_only:
        parser.error('execute and cleanup-only conflict')
    directory = args.state_dir.resolve()
    try:
        plan = load_bundle(directory)
        if ((args.execute and args.allow_cloud_test_directory != plan['folder'])
                or (args.allow_cloud_test_directory and args.allow_cloud_test_directory != plan['folder'])):
            parser.error('execute requires explicit approval of the exact prepared cloud directory')
        if not args.execute and not args.cleanup_only:
            print(json.dumps({'local_preflight_passed': True, 'live_validation_passed': False,
                              'network_calls': 0, 'created_sessions': 0, 'cloud_writes': 0}))
            return 0
        lock_fd = os.open(directory / 'executor.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
        with os.fdopen(lock_fd, 'w'):
            if not stat.S_ISREG(os.fstat(lock_fd).st_mode) or os.fstat(lock_fd).st_mode & 0o077:
                raise ValidationError('unsafe_executor_lock')
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            report = execute(directory, plan, args.cleanup_only)
        print(json.dumps(report))
        if not report['passed']:
            print('保留私有账本；先核查未知操作，恢复入口仅 --cleanup-only。', file=sys.stderr)
        return 0 if report['passed'] else 1
    except (ValidationError, OSError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({'passed': False, 'error_class': str(error) if isinstance(error, (ValidationError, ObservationError)) else type(error).__name__}))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
