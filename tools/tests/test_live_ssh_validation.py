import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import Mock

SPEC = importlib.util.spec_from_file_location('live_ssh', Path(__file__).resolve().parents[1] / 'validate_live_ssh.py')
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


def ledger(endpoint='owned'):
    return {'session': 'unique-session', 'baseline_endpoints': ['existing'], 'endpoint': endpoint,
            'allocation_requested': True}


class LiveSSHSafetyTests(unittest.TestCase):
    @unittest.skipUnless(os.name == 'posix', 'Unix PTY')
    def test_interactive_probe_rejects_echoed_command_without_remote_result(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / 'echo.py'
            script.write_text('import sys\nprint(sys.stdin.readline(),end="")\n')
            client = Mock(prefix=[sys.executable, str(script)], env=dict(os.environ))
            diagnostic = Path(directory) / 'diagnostic.json'
            with self.assertRaisesRegex(live.ValidationError, 'interactive_shell_result_missing'):
                live.probe_interactive(client, 'synthetic', Path('/synthetic-key'), 'owned', timeout=3,
                                       diagnostic=diagnostic)
            self.assertEqual(diagnostic.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(diagnostic.read_text())['exit_code'], 0)

    @unittest.skipUnless(os.name == 'posix', 'Unix PTY')
    def test_interactive_probe_attaches_all_streams_to_a_pty(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / 'pty.py'
            script.write_text('import ast,json,os,shlex,sys\n'
                              'assert all(os.isatty(i) for i in range(3))\n'
                              'code=shlex.split(sys.stdin.readline())[2]\n'
                              'tree=ast.parse(code)\n'
                              'marker=next(n.value for n in ast.walk(tree) if isinstance(n,ast.Constant) and isinstance(n.value,str) and n.value.startswith("COLAB_PTY_"))\n'
                              'print(marker+json.dumps({"tty":[True]*3,"cwd":"/content","ssh_tty":True}))\n')
            client = Mock(prefix=[sys.executable, str(script)], env=dict(os.environ))
            live.probe_interactive(client, 'synthetic', Path('/synthetic-key'), 'owned', timeout=3)

    @unittest.skipUnless(os.name == 'posix', 'Unix signals')
    def test_signal_probe_waits_for_remote_marker_then_observes_exit_143(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / 'synthetic_signal_target.py'
            script.write_text('import ast,signal,sys,time\n'
                              'signal.signal(signal.SIGTERM,lambda *args:sys.exit(143))\n'
                              'tree=ast.parse(sys.argv[-1])\n'
                              'marker=next(n.value for n in ast.walk(tree) if isinstance(n,ast.Constant) and isinstance(n.value,str) and n.value.startswith("COLAB_SIGNAL_"))\n'
                              'print(marker,flush=True)\ntime.sleep(60)\n')
            client = Mock(prefix=[sys.executable, str(script)], env=dict(os.environ))
            live.probe_command_signal(client, 'synthetic', Path('/synthetic-key'), 'owned', timeout=3)

    @unittest.skipUnless(os.name == 'posix', 'Unix signals')
    def test_signal_probe_refuses_eof_without_remote_marker(self):
        client = Mock(prefix=[sys.executable, '-c', 'pass'], env=dict(os.environ))
        with self.assertRaisesRegex(live.ValidationError, 'signal_remote_marker_eof'):
            live.probe_command_signal(client, 'synthetic', Path('/synthetic-key'), 'owned', timeout=3)

    def test_json_exit_zero_with_account_error_cannot_authorize_allocation(self):
        client = live.Client(Path('/colab'), Path('/config'), 'default', 'oauth2', 'file', {})
        client.call = Mock(return_value=subprocess.CompletedProcess([], 0, json.dumps(
            {'account': 'default', 'error': 'synthetic authentication failure', 'assignments': None}).encode(), b''))
        with self.assertRaises(live.ValidationError):
            client.inventory()
        self.assertEqual(client.call.call_args.args[0], ['sessions', '--json'])
        self.assertEqual(client.call.call_count, 1)

    def test_inventory_refuses_different_account_even_when_valid(self):
        client = live.Client(Path('/colab'), Path('/config'), 'selected', 'oauth2', 'file', {})
        client.call = Mock(return_value=subprocess.CompletedProcess([], 0, b'{"account":"other","error":null,"assignments":[]}', b''))
        with self.assertRaises(live.ValidationError):
            client.inventory()

    def test_inventory_refuses_non_object_assignment_rows(self):
        client = live.Client(Path('/colab'), Path('/config'), 'selected', 'oauth2', 'file', {})
        for row in (None, [], 'endpoint'):
            client.call = Mock(return_value=subprocess.CompletedProcess([], 0, json.dumps(
                {'account': 'selected', 'error': None, 'assignments': [row]}).encode(), b''))
            with self.subTest(row=row), self.assertRaises(live.ValidationError):
                client.inventory()

    def test_cleanup_always_passes_captured_endpoint_and_verifies_backend_absence(self):
        client = Mock()
        client.call.return_value = subprocess.CompletedProcess([], 1, b'', b'local state already absent')
        client.inventory.return_value = {'existing', 'another-new-vm'}
        state = ledger()
        self.assertEqual(live.cleanup(client, state), 'confirmed_released')
        client.call.assert_called_once_with(['stop', '-s', 'unique-session', '--colab-expected-endpoint', 'owned'])
        self.assertEqual(state['other_new_endpoints'], ['another-new-vm'])
        client.inventory.side_effect = live.ValidationError('inventory failed')
        with self.assertRaises(live.ValidationError):
            live.cleanup(client, state)

    def test_cleanup_never_stops_baseline_or_unknown_resources(self):
        client = Mock()
        self.assertEqual(live.cleanup(client, ledger('existing')), 'ownership_refused')
        self.assertEqual(live.cleanup(client, ledger(None)), 'allocation_unknown')
        client.call.assert_not_called()
        client.inventory.assert_not_called()

    def test_allocation_capture_rejects_baseline_or_wrong_identity(self):
        for value in ({'endpoint': 'existing', 'name': 'unique-session', 'accelerator': 'NONE'},
                      {'endpoint': 'new', 'name': 'other', 'accelerator': 'NONE'}):
            state = ledger(None)
            with self.assertRaises(live.ValidationError):
                live.capture_allocation(value, state)
            self.assertIsNone(state['endpoint'])

    def test_unexpected_gpu_is_recorded_for_cleanup_but_not_accepted_as_cpu(self):
        state = ledger(None)
        live.capture_allocation({'endpoint': 'new-owned', 'name': 'unique-session', 'accelerator': 'T4'}, state)
        self.assertFalse(state['is_cpu'])
        client = Mock()
        client.call.return_value = subprocess.CompletedProcess([], 0, b'', b'')
        client.inventory.return_value = {'existing'}
        self.assertEqual(live.cleanup(client, state), 'confirmed_released')
        client.call.assert_called_once_with(['stop', '-s', 'unique-session', '--colab-expected-endpoint', 'new-owned'])

    @unittest.skipUnless(os.name == 'posix', 'Unix file permissions')
    def test_private_ledger_is_atomic_and_permission_restricted(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'ledger.json'
            live.private_json(path, {'endpoint': 'synthetic-owned'})
            live.private_json(path, {'endpoint': 'updated-owned'})
            self.assertEqual(json.loads(path.read_text())['endpoint'], 'updated-owned')
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(list(path.parent.iterdir()), [path])

    @unittest.skipUnless(os.name == 'posix', 'Unix process groups')
    def test_timed_out_command_reaps_its_owned_parent_and_child_group(self):
        with tempfile.TemporaryDirectory() as directory:
            script, marker = Path(directory) / 'owned.py', Path(directory) / 'child.pid'
            script.write_text('import subprocess,sys,time,signal\nfrom pathlib import Path\n'
                              'child=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"])\n'
                              'def stop(*args):\n child.wait(timeout=3)\n sys.exit(0)\n'
                              'signal.signal(signal.SIGTERM,stop)\n'
                              'Path(sys.argv[1]).write_text(str(child.pid))\ntime.sleep(60)\n')
            client = live.Client(Path('/unused'), Path('/config'), 'default', 'oauth2', 'file', dict(os.environ))
            client.prefix = [sys.executable, str(script), str(marker)]
            pid = None
            needs_cleanup = True
            try:
                with self.assertRaises(subprocess.TimeoutExpired):
                    client.call([], timeout=1)
                pid = int(marker.read_text())
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
                needs_cleanup = False
            finally:
                if needs_cleanup and pid is None and marker.exists():
                    pid = int(marker.read_text())
                if needs_cleanup and pid is not None:
                    try:
                        os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass


@unittest.skipUnless(os.name == 'posix' and Path('/usr/bin/ssh-keygen').exists(), 'Unix SSH tooling')
class LiveSSHWorkflowTests(unittest.TestCase):
    """Run the real harness entry point against an owned, synthetic CLI process.

    The model never imports the product or opens sockets; only ssh-keygen is real.
    It records wire-independent ownership and recovery decisions across processes.
    """
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.config = self.root / 'config'
        self.config.mkdir()
        self.state = self.root / 'private-state'
        self.model = self.root / 'model.json'
        self.model.write_text(json.dumps({'calls': [], 'allocated': False, 'new_count': 0}))
        self.binary = self.root / 'synthetic-colab'
        self.binary.write_text('#!' + sys.executable + '\n' + textwrap.dedent('''\
            import ast, json, os, sys
            from pathlib import Path
            path = Path(os.environ['HARNESS_MODEL'])
            model = json.loads(path.read_text())
            args = sys.argv[1:]
            model['calls'].append(args)
            options = {}
            while args and args[0].startswith('--'):
                options[args[0]] = args[1]
                args = args[2:]
            command = args[0]
            scenario = os.environ['HARNESS_SCENARIO']
            config = Path(options['--config-dir'])
            journal = config / 'accounts' / 'synthetic' / 'control_operation.json'
            code = 0
            value = None
            if command == 'account':
                value = {'account': 'synthetic', 'auth': {'provider': 'oauth2'}}
            elif command == 'sessions':
                value = {'account': 'synthetic', 'error': 'synthetic denied' if scenario == 'denied' else None,
                         'assignments': [{'endpoint': 'baseline'}] +
                         ([{'endpoint': 'owned'}] if model['allocated'] else [])}
            elif command == 'new':
                model['new_count'] += 1
                name = args[args.index('-s') + 1]
                model['allocated'] = True
                value = {'name': name, 'endpoint': 'owned', 'accelerator': 'NONE'}
                if scenario == 'unknown' and model['new_count'] == 1:
                    journal.parent.mkdir(parents=True, exist_ok=True)
                    journal.write_text(json.dumps({'operation': 'allocate', 'name': name}))
                    value, code = None, 1
                else:
                    journal.unlink(missing_ok=True)
            elif command == 'stop':
                assert args[args.index('--colab-expected-endpoint') + 1] == 'owned'
                if scenario == 'stop-fails':
                    code = 1
                else:
                    model['allocated'] = False
            elif command == 'ssh':
                assert args[args.index('--colab-expected-endpoint') + 1] == 'owned'
                if scenario not in ('success', 'binary-fails'):
                    # Fail after allocation, before any remote code.
                    code = 1
                elif '--proxy-mode' in args:
                    assert sys.stdin.buffer.readline().startswith(b'SSH-2.0-')
                    sys.stdout.buffer.write(b'SSH-2.0-synthetic-server\\r\\n')
                    sys.stdout.buffer.flush()
                    assert sys.stdin.buffer.read() == b''
                else:
                    remote = args[args.index('--') + 1:]
                    if remote[0] == 'sh':
                        assert remote == ['sh', '-c', 'exit 23']
                        code = 23
                    elif 'os.getuid()' in remote[2]:
                        # Inspect synthetic arguments; do not execute supplied code.
                        node = next(node for node in ast.walk(ast.parse(remote[2])) if isinstance(node, ast.Dict))
                        marker = ast.literal_eval(node.values[0])
                        value = {'marker': marker, 'uid': 0, 'cwd': '/content'}
                    elif 'sys.argv' in remote[2]:
                        value = remote[3:]
                    else:
                        assert 'sys.stdin.buffer.read()' in remote[2]
                        if scenario == 'binary-fails':
                            print('synthetic-private-server-diagnostic', file=sys.stderr)
                            code = 255
                        else:
                            sys.stdout.buffer.write(sys.stdin.buffer.read())
            else:
                raise AssertionError(command)
            path.write_text(json.dumps(model))
            if value is not None:
                print(json.dumps(value))
            sys.exit(code)
            '''))
        self.binary.chmod(0o700)

    def run_harness(self, scenario, *args):
        result = subprocess.run([sys.executable, str(SPEC.origin), '--binary', str(self.binary),
                                 '--config-dir', str(self.config), '--state-dir', str(self.state), *args],
                                env=dict(os.environ, HARNESS_MODEL=str(self.model), HARNESS_SCENARIO=scenario,
                                         COLAB_ACCOUNT='must-not-select-this', COLAB_AUTH='adc'),
                                capture_output=True, timeout=20)
        report = json.loads((self.state / 'report.json').read_text())
        self.assertEqual(json.loads(result.stdout), report, result.stderr)
        self.assertFalse((self.state / 'keys').exists())
        self.assertEqual((self.state / 'report.json').stat().st_mode & 0o777, 0o600)
        return result, report, json.loads(self.model.read_text())

    def test_preflight_reports_success_without_allocation_or_ssh(self):
        result, report, model = self.run_harness('ready')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report['passed'])
        self.assertTrue(report['preflight_only'])
        self.assertEqual(report['created_sessions'], 0)
        self.assertEqual(len(model['calls']), 2)
        self.assertFalse(model['allocated'])

    def test_account_error_exit_zero_stops_before_allocation(self):
        result, report, model = self.run_harness('denied', '--execute')
        self.assertEqual(result.returncode, 1)
        self.assertFalse(report['passed'])
        self.assertEqual(report['error_class'], 'inventory_credentials_or_account_error')
        self.assertEqual(model['new_count'], 0)

    def test_failed_handshake_releases_only_owned_vm_and_removes_private_key(self):
        result, report, model = self.run_harness('ready', '--execute')
        self.assertEqual(result.returncode, 1)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(report['created_sessions'], 1)
        self.assertEqual(model['new_count'], 1)
        self.assertFalse(model['allocated'])
        self.assertEqual(report['checks'], [])
        for call in model['calls'][1:]:
            self.assertEqual(call[call.index('--account') + 1], 'synthetic')
            self.assertEqual(call[call.index('--token-storage') + 1], 'file')

    def test_success_runs_all_checks_then_confirms_owned_vm_released(self):
        result, report, model = self.run_harness('success', '--execute')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report['passed'])
        self.assertEqual([check['name'] for check in report['checks']],
                         ['raw_proxy_banner_and_eof', 'identity_and_workdir', 'quoted_argv',
                          'binary_stdio', 'remote_exit_code'])
        self.assertTrue(all(check['passed'] for check in report['checks']))
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(model['new_count'], 1)
        self.assertFalse(model['allocated'])

    def test_unknown_allocation_reconciles_same_name_then_cleans_without_ssh(self):
        result, report, model = self.run_harness('unknown', '--execute')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertFalse(model['allocated'])
        calls = model['calls']
        new_calls = [call for call in calls if 'new' in call]
        self.assertEqual(len(new_calls), 2)
        self.assertEqual(new_calls[0], new_calls[1])
        self.assertFalse(any('ssh' in call for call in calls))
        self.assertFalse((self.config / 'accounts/synthetic/control_operation.json').exists())

    def test_failed_command_keeps_raw_diagnostics_private_and_still_releases(self):
        result, report, model = self.run_harness('binary-fails', '--execute')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(report['error_class'], 'ssh_check_failed:binary_stdio')
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertFalse(model['allocated'])
        raw = self.state / 'failed-check.json'
        self.assertEqual(raw.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(raw.read_text())['exit_code'], 255)
        self.assertNotIn('synthetic-private-server-diagnostic', (self.state / 'report.json').read_text())

    def test_cleanup_only_recovers_failed_stop_without_new_allocation_or_ssh(self):
        first, failed, model = self.run_harness('stop-fails', '--execute')
        self.assertEqual(first.returncode, 1)
        self.assertEqual(failed['cleanup'], 'pending')
        self.assertTrue(model['allocated'])
        call_count = len(model['calls'])
        result, report, model = self.run_harness('ready', '--cleanup-only')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertFalse(model['allocated'])
        self.assertEqual(model['new_count'], 1)
        self.assertEqual(len(model['calls'][call_count:]), 2)
        self.assertFalse(any('new' in call or 'ssh' in call for call in model['calls'][call_count:]))


if __name__ == '__main__':
    unittest.main()
