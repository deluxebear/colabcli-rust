import contextlib
import ast
import fcntl
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import prepare_live_drive_validation as prep
import validate_live_drive as live


class Model:
    """Synthetic CLI replies and native-state commits; no subprocess or sockets."""
    def __init__(self, directory, plan, scenario='success'):
        self.directory, self.plan, self.scenario = directory, plan, scenario
        self.root = Path(plan['config_dir'])
        self.calls = []
        self.present = False
        self.mount_count = 0
        self.cloud = False
        self.stops = 0

    @property
    def states(self):
        return self.root / 'accounts/compute/sessions.json'

    def state(self):
        return json.loads(self.states.read_text())[self.plan['session']]

    def save(self, state):
        self.states.write_text(json.dumps({self.plan['session']: state}))

    def inventory(self):
        self.calls.append(['inventory'])
        return {'baseline'} | ({'owned'} if self.present else set())

    def call(self, args, **kwargs):
        self.calls.append(list(args))
        runtime = args[0] == '--colab-runtime-endpoint'
        if runtime:
            assert args[1] == 'owned'
            args = args[2:]
        code, value = 0, None
        if args[:2] == ['drive', 'token']:
            assert not runtime
            value = {'account': 'storage', 'user_id': 'other' if self.scenario == 'wrong-identity' else 'synthetic-user',
                     'access_token': 'sensitive-access-NEVER-SAVE', 'scope': 'https://www.googleapis.com/auth/drive'}
        elif args[0] == 'new':
            assert not runtime
            self.present = True
            self.save({'name': self.plan['session'], 'endpoint': 'owned'})
            value = {'name': self.plan['session'], 'endpoint': 'owned', 'accelerator': 'NONE'}
            journal = self.root / 'accounts/compute/control_operation.json'
            if self.scenario == 'allocation-lost' and not journal.exists():
                journal.write_text(json.dumps({'operation': 'allocate', 'name': self.plan['session']}))
                raise subprocess.TimeoutExpired('synthetic', 1)
            journal.unlink(missing_ok=True)
            if self.scenario == 'gpu':
                value['accelerator'] = 'T4'
        elif args[:2] == ['drive', 'mount']:
            assert runtime
            self.mount_count += 1
            state = self.state()
            readonly = '--readonly' in args
            owner = f'{self.mount_count:032x}'
            lease = {'compute_account': 'compute', 'drive_account': 'storage', 'session': self.plan['session'],
                     'endpoint': 'owned', 'identity': self.plan['drive_identity_sha256'],
                     'request': {'operation_id': owner, 'readonly': readonly}}
            state['drive_mounts'] = {self.plan['mountpoint']: lease}
            self.save(state)
            value = self.mount_reply(state)
            if self.scenario == 'mount-lost' and self.mount_count == 1:
                state['drive_plan'] = {'pending': True}
                self.save(state)
                raise subprocess.TimeoutExpired('synthetic', 1)
        elif args[:2] == ['drive', 'status']:
            assert runtime
            state = self.state()
            value = self.mount_reply(state) if state.get('drive_mounts') else self.unmounted()
        elif args[:2] == ['drive', 'umount']:
            assert runtime
            state = self.state()
            if self.scenario == 'umount-failed':
                state['drive_plan'] = {'credentials_pending': True}
                self.save(state)
                code, value = 1, {'ok': False, 'mounted': False, 'stage': 'credentials_pending'}
            else:
                if self.scenario != 'umount-reply-with-lease':
                    state.pop('drive_mounts', None)
                state.pop('drive_plan', None)
                self.save(state)
                value = self.unmounted()
        elif args[0] == 'exec':
            assert runtime
            phase = ast.literal_eval(kwargs['data'].decode().splitlines()[1].split('=', 1)[1].strip())
            self.calls.append(['payload', phase])
            if phase == 'create':
                self.cloud = True
            elif phase == 'cleanup':
                self.cloud = False
            status = {'create': 'created_and_read_back', 'readonly': 'write_denied', 'cleanup': 'removed'}[phase]
            result = {'phase': phase, 'status': status, 'original_bytes_unchanged': True}
            value = [{'status': 'succeeded', 'outputs': [{'output_type': 'stream', 'name': 'stdout', 'text': json.dumps(result)+'\n'}]}]
            if self.scenario == 'readonly-writable' and phase == 'readonly':
                code, value = 1, [{'status': 'failed', 'outputs': []}]
            if self.scenario == 'exec-lost' and phase == 'create':
                state = self.state(); state['running'] = 'uncertain'; self.save(state)
                code, value = 1, [{'status': 'unknown', 'outputs': []}]
            if self.scenario == 'unknown-zero' and phase == 'create':
                state = self.state(); state['running'] = 'uncertain'; self.save(state)
                code, value = 0, [{'status': 'unknown', 'outputs': []}]
            if self.scenario == 'bad-probe-json' and phase == 'create':
                value[0]['outputs'][0]['text'] = 'not a probe result\n'
        elif args[0] == 'stop':
            assert not runtime and args[-2:] == ['--colab-expected-endpoint', 'owned']
            assert not self.cloud and not self.state().get('drive_mounts')
            self.stops += 1
            if self.scenario == 'stop-first-fails' and self.stops == 1:
                code = 1
            else:
                self.present = False
                self.states.write_text('{}')
        else:
            raise AssertionError(args)
        return subprocess.CompletedProcess([], code, json.dumps(value).encode() if value is not None else b'', b'')

    def unmounted(self):
        return {'ok': True, 'mounted': False, 'stage': 'finished', 'mountpoint': self.plan['mountpoint']}

    def mount_reply(self, state):
        lease = state['drive_mounts'][self.plan['mountpoint']]
        return {'ok': True, 'mounted': True, 'managed': True, 'stage': 'mounted', 'mode': 'shim',
                'readonly': lease['request']['readonly'], 'owner_operation_id': lease['request']['operation_id'],
                'identity': lease['identity'], 'mountpoint': self.plan['mountpoint'], 'drive_alive': True, 'helper_alive': True}


class ExecutorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / 'config'
        (self.root / 'accounts/storage').mkdir(parents=True)
        (self.root / 'accounts/compute').mkdir()
        (self.root / 'accounts.json').write_text(json.dumps({'accounts': {'compute': {}, 'storage': {}}}))
        self.credential = self.root / 'accounts/storage/dfs_credentials.json'
        self.credential.write_text(json.dumps({'client_id': 'synthetic-client', 'refresh_token': 'sensitive-refresh-NEVER-SAVE',
                                              'user_id': 'synthetic-user', 'scopes': ['https://www.googleapis.com/auth/drive']}))
        self.binary = self.base / 'synthetic-binary'; self.binary.write_bytes(b'never executed')
        self.directory = self.base / 'prepared'
        prep.prepare(self.root, self.directory, 'compute', 'storage', self.binary)
        self.plan = live.load_bundle(self.directory)

    def run_model(self, model, cleanup=False):
        def factory(binary, root, account, provider, mode, env):
            self.assertFalse(any(k.startswith('COLAB_') for k in env))
            self.assertEqual((account, provider, mode), ('compute', 'oauth2', 'file'))
            return model
        class Observer:
            def __init__(inner, token):
                self.assertEqual(token, 'sensitive-access-NEVER-SAVE')
            def verify_identity(inner):
                if model.scenario == 'api-identity-failed':
                    raise live.ObservationError('drive_api_root_owner_mismatch')
                return {'same_token_drive_ownership_verified': True}
            def inspect(inner, folder, files):
                self.assertEqual(folder, self.plan['folder'])
                self.assertEqual(len(files), 2)
                self.assertFalse(model.state().get('drive_mounts'))
                if model.scenario == 'api-bytes-failed':
                    raise live.ObservationError('drive_api_probe_bytes_mismatch')
                return {'server_bytes_verified': True}
            def verify_removed(inner):
                self.assertFalse(model.cloud)
                self.assertFalse(model.state().get('drive_mounts'))
                if model.scenario == 'api-cleanup-unconfirmed':
                    raise live.ObservationError('drive_api_probe_object_still_live_or_changed')
                return {'server_cleanup_verified': True, 'trashed_objects': 3, 'missing_objects': 0}
        report = live.execute(self.directory, self.plan, cleanup, factory, Observer)
        for name in ['execution.json', 'execution-report.json']:
            text = (self.directory / name).read_text()
            self.assertNotIn('sensitive-access-NEVER-SAVE', text)
            self.assertNotIn('sensitive-refresh-NEVER-SAVE', text)
            self.assertEqual((self.directory / name).stat().st_mode & 0o777, 0o600)
        return report

    def test_success_three_mounts_recovers_credentials_cleans_cloud_then_guarded_stop(self):
        model = Model(self.directory, self.plan)
        report = self.run_model(model)
        self.assertTrue(report['passed'], report)
        self.assertTrue(report['credential_userinfo_verified'])
        self.assertTrue(report['same_token_drive_ownership_verified'])
        self.assertTrue(report['server_bytes_verified'])
        self.assertTrue(report['server_cleanup_verified'])
        self.assertTrue(report['readonly_verified'])
        self.assertTrue(report['cloud_cleanup_confirmed'])
        self.assertFalse(report['independent_drive_api_identity_verified'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual((model.mount_count, model.stops), (3, 1))
        self.assertFalse(model.cloud)
        self.assertTrue(self.run_model(model, True)['passed'])
        self.assertEqual(model.stops, 1)

    def test_cloud_cleanup_unconfirmed_is_not_full_success_even_after_vm_release(self):
        model = Model(self.directory, self.plan, 'api-cleanup-unconfirmed')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertFalse(report['server_cleanup_verified'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(model.stops, 1)
        self.assertFalse(json.loads((self.directory / 'execution.json').read_text()).get('validation_steps_passed', False))

    def test_drive_api_identity_failure_prevents_allocation(self):
        model = Model(self.directory, self.plan, 'api-identity-failed')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['created_sessions'], 0)
        self.assertFalse(any(c[0] == 'new' for c in model.calls))

    def test_drive_api_bytes_failure_uses_bounded_cleanup_and_releases(self):
        model = Model(self.directory, self.plan, 'api-bytes-failed')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertFalse(report['server_bytes_verified'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertFalse(model.cloud)
        self.assertEqual(model.mount_count, 2)

    def test_wrong_fresh_identity_never_allocates_or_exports_access_token(self):
        model = Model(self.directory, self.plan, 'wrong-identity')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['created_sessions'], 0)
        self.assertEqual(report['cleanup'], 'not_allocated')
        self.assertFalse(any(c[0] == 'new' for c in model.calls))

    def test_known_readonly_failure_still_cleans_owned_files_and_releases(self):
        model = Model(self.directory, self.plan, 'readonly-writable')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertTrue(report['cloud_cleanup_confirmed'])
        self.assertFalse(report['readonly_verified'])
        self.assertFalse(model.cloud)

    def test_unknown_exec_is_never_replayed_or_released_until_explicit_restart(self):
        model = Model(self.directory, self.plan, 'exec-lost')
        report = self.run_model(model)
        self.assertEqual(report['cleanup'], 'pending_recovery')
        count = len(model.calls)
        self.assertEqual(self.run_model(model, True)['cleanup'], 'pending_recovery')
        self.assertEqual(len(model.calls), count)
        self.assertEqual(model.stops, 0)
        state = model.state(); state.pop('running'); state['kernel_generation'] = 'confirmed-explicit-restart'; model.save(state)
        model.scenario = 'success'
        report = self.run_model(model, True)
        self.assertTrue(report['passed'], report)
        creates = [c for c in model.calls if c == ['payload', 'create']]
        self.assertEqual(len(creates), 1)

    def test_unknown_mount_is_not_replayed_and_keeps_vm_for_native_recovery(self):
        model = Model(self.directory, self.plan, 'mount-lost')
        report = self.run_model(model)
        self.assertEqual(report['cleanup'], 'pending_recovery')
        self.assertEqual(model.mount_count, 1)
        self.assertEqual(model.stops, 0)
        state = model.state(); state.pop('drive_plan'); model.save(state)
        model.scenario = 'success'
        report = self.run_model(model, True)
        self.assertTrue(report['passed'], report)
        self.assertEqual(model.mount_count, 1)

    def test_unconfirmed_credential_recovery_keeps_vm_and_cloud_data(self):
        model = Model(self.directory, self.plan, 'umount-failed')
        report = self.run_model(model)
        self.assertEqual(report['cleanup'], 'pending_recovery')
        self.assertTrue(model.cloud)
        self.assertEqual(model.stops, 0)
        state = model.state(); state['drive_plan'] = {}; model.save(state)
        count = len(model.calls)
        self.assertEqual(self.run_model(model, True)['cleanup'], 'pending_recovery')
        self.assertEqual(len(model.calls), count)

    def test_failed_stop_can_resume_without_replaying_cloud_operations(self):
        model = Model(self.directory, self.plan, 'stop-first-fails')
        self.assertEqual(self.run_model(model)['cleanup'], 'pending_recovery')
        mounts = model.mount_count
        report = self.run_model(model, True)
        self.assertTrue(report['passed'], report)
        self.assertEqual(model.mount_count, mounts)
        self.assertEqual(model.stops, 2)

    def test_unknown_allocation_reconciles_same_name_then_only_releases(self):
        model = Model(self.directory, self.plan, 'allocation-lost')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        creates = [x for x in model.calls if x[0] == 'new']
        self.assertEqual(len(creates), 2)
        self.assertEqual(creates[0], creates[1])
        self.assertEqual(model.mount_count, 0)

    def test_unexpected_gpu_is_captured_for_cleanup_without_drive_access(self):
        model = Model(self.directory, self.plan, 'gpu')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(model.mount_count, 0)

    def test_default_cli_preflight_has_no_client_or_execution_ledger(self):
        with patch.object(sys, 'argv', ['validate', '--state-dir', str(self.directory)]), patch.object(live, 'Client', side_effect=AssertionError('no CLI')), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(live.main(), 0)
        self.assertFalse((self.directory / 'execution.json').exists())

    def test_exact_cloud_scope_is_required_before_client_construction(self):
        with patch.object(sys, 'argv', ['validate', '--state-dir', str(self.directory), '--execute', '--allow-cloud-test-directory', 'another-folder']), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                live.main()
            self.assertEqual(error.exception.code, 2)
        self.assertFalse((self.directory / 'execution.json').exists())

    def test_binary_payload_or_identity_changes_invalidate_prepared_bundle(self):
        original = self.binary.read_bytes(); self.binary.write_bytes(b'changed')
        with self.assertRaisesRegex(live.ValidationError, 'prepared_binary_changed'):
            live.load_bundle(self.directory)
        self.binary.write_bytes(original)
        payload = self.directory / 'create.py'; code = payload.read_text(); payload.write_text(code+'\nprint("extra")')
        with self.assertRaisesRegex(live.ValidationError, 'prepared_payload_changed'):
            live.load_bundle(self.directory)
        payload.write_text(code)
        value = json.loads(self.credential.read_text()); value['user_id'] = 'another'; self.credential.write_text(json.dumps(value))
        with self.assertRaisesRegex(live.ValidationError, 'prepared_drive_identity_changed'):
            live.load_bundle(self.directory)

    def test_second_execute_refuses_before_any_call(self):
        model = Model(self.directory, self.plan)
        self.run_model(model)
        count = len(model.calls)
        with self.assertRaisesRegex(live.ValidationError, 'execution_already_started_use_cleanup_only'):
            self.run_model(model)
        self.assertEqual(len(model.calls), count)

    def test_same_name_replacement_never_receives_cleanup_or_runtime_commands(self):
        model = Model(self.directory, self.plan, 'exec-lost')
        self.run_model(model)
        state = model.state(); state['endpoint'] = 'replacement'; model.save(state)
        count = len(model.calls)
        self.assertEqual(self.run_model(model, True)['cleanup'], 'pending_recovery')
        self.assertEqual(len(model.calls), count)
        self.assertEqual(model.stops, 0)

    def test_simultaneous_executor_is_refused_before_any_execution(self):
        descriptor = os.open(self.directory / 'executor.lock', os.O_CREAT | os.O_RDWR, 0o600)
        self.addCleanup(os.close, descriptor)
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with patch.object(sys, 'argv', ['validate', '--state-dir', str(self.directory), '--execute',
                                      '--allow-cloud-test-directory', self.plan['folder']]), \
                patch.object(live, 'execute', side_effect=AssertionError('must not execute')), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(live.main(), 1)
        self.assertFalse((self.directory / 'execution.json').exists())

    def test_exact_authorized_scope_dispatches_only_after_bundle_validation(self):
        with patch.object(sys, 'argv', ['validate', '--state-dir', str(self.directory), '--execute',
                                      '--allow-cloud-test-directory', self.plan['folder']]), \
                patch.object(live, 'execute', return_value={'passed': True}) as execute, \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(live.main(), 0)
        self.assertEqual(execute.call_args.args[1]['folder'], self.plan['folder'])

    def test_terminal_exec_with_bad_probe_output_can_only_run_bounded_cleanup(self):
        model = Model(self.directory, self.plan, 'bad-probe-json')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(model.calls.count(['payload', 'create']), 1)
        self.assertEqual(model.calls.count(['payload', 'cleanup']), 1)

    def test_changed_payload_is_rejected_at_dispatch_after_initial_bundle_check(self):
        model = Model(self.directory, self.plan)
        ledger = {'plan_sha256': live.digest(self.directory / 'plan.json'), 'session': self.plan['session'],
                  'baseline_endpoints': ['baseline'], 'completed': [], 'inflight': None}
        engine = live.Executor(self.directory, self.plan, ledger, model)
        engine.allocate()
        (self.directory / 'create.py').write_text('raise Exception("unreviewed")')
        with self.assertRaisesRegex(live.ValidationError, 'payload_changed_before_dispatch'):
            engine.probe('create')
        self.assertFalse(any('exec' in x for x in model.calls))
        self.assertEqual(engine.cleanup(), 'confirmed_released')

    def test_exit_zero_cannot_hide_unknown_execution(self):
        model = Model(self.directory, self.plan, 'unknown-zero')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'pending_recovery')
        self.assertEqual(model.stops, 0)

    def test_successful_unmount_reply_is_insufficient_while_lease_remains(self):
        model = Model(self.directory, self.plan, 'umount-reply-with-lease')
        report = self.run_model(model)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'pending_recovery')
        self.assertEqual(model.stops, 0)


if __name__ == '__main__':
    unittest.main()
